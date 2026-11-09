//! The connection loop: dial, say hello, store the rotated token, serve,
//! and when the socket goes, back off and dial again.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rmcp::ServerHandler;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream};

use crate::frames::{json_frames, FrameSink, FrameStream};
use crate::gate::Gated;
use crate::wire::{
    AppFrame, Auth, Hello, LuminaFrame, Refused, RefusedReason, Unlink, Welcome, VERSION,
};
use crate::{Command, Linked, PairError, Shared, Status};

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 1 << 20;

type Socket = MaybeTlsStream<TcpStream>;
type PairReply = oneshot::Sender<Result<Linked, PairError>>;

enum Failure {
    Refused(Refused),
    Failed(String),
}

fn tls() -> Connector {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(config)
    });
    Connector::Rustls(config.clone())
}

/// Open the socket, send `first`, read Lumina's answer.
async fn handshake(
    shared: &Shared,
    endpoint: &str,
    first: AppFrame,
) -> Result<(FrameSink<Socket, AppFrame>, FrameStream<Socket, LuminaFrame>, Welcome), Failure> {
    use futures::{SinkExt, StreamExt};
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let attempt = async {
        let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
            endpoint,
            Some(config),
            true,
            Some(tls()),
        )
        .await
        .map_err(|e| Failure::Failed(e.to_string()))?;
        let (mut sink, mut stream) =
            json_frames::<_, AppFrame, LuminaFrame>(ws, Some(shared.liveness));
        sink.send(first).await.map_err(|e| Failure::Failed(e.to_string()))?;
        match stream.next().await {
            Some(LuminaFrame::Welcome(welcome)) => Ok((sink, stream, welcome)),
            Some(LuminaFrame::Refused(refused)) => Err(Failure::Refused(refused)),
            None => Err(Failure::Failed("Lumina closed the connection without answering".into())),
        }
    };
    match tokio::time::timeout(shared.handshake_timeout, attempt).await {
        Ok(result) => result,
        Err(_) => Err(Failure::Failed("Lumina did not answer in time".into())),
    }
}

fn hello(shared: &Shared, auth: Auth) -> AppFrame {
    AppFrame::Hello(Hello {
        v: VERSION,
        app: shared.app.clone(),
        version: shared.version.clone(),
        label: shared.label.clone(),
        consent: shared.consent().report(),
        auth,
    })
}

/// `delay` ±25 %, from the clock's low bits: enough to spread a fleet of
/// devices reconnecting after Lumina restarts.
fn jittered(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let factor = 0.75 + f64::from(nanos % 1000) / 2000.0;
    delay.mul_f64(factor)
}

pub(crate) async fn run<S, F>(
    shared: Arc<Shared>,
    factory: F,
    mut commands: mpsc::UnboundedReceiver<Command>,
) where
    S: ServerHandler,
    F: Fn() -> S + Send + Sync + 'static,
{
    let mut delay = shared.backoff.min;
    let mut pending: Option<Command> = None;
    let mut pair: Option<(String, PairReply)> = None;
    // Set by a refusal that retrying cannot fix; cleared by a new pairing.
    let mut parked = false;

    loop {
        match pending.take() {
            Some(Command::Shutdown) => return,
            Some(Command::Unlink(done)) => {
                if let Some((_, reply)) = pair.take() {
                    let _ = reply.send(Err(PairError::Interrupted));
                }
                unlink(&shared).await;
                parked = false;
                let _ = done.send(());
                continue;
            }
            Some(Command::Pair(code, reply)) => {
                if let Some((_, old)) = pair.replace((code, reply)) {
                    let _ = old.send(Err(PairError::Interrupted));
                }
                parked = false;
            }
            None => {}
        }

        let stored = shared.lock_stored().clone();
        let linked = stored.credentials().is_some();
        if pair.is_none() && (!linked || parked) {
            if !linked
                && matches!(*shared.status.borrow(), Status::Connecting | Status::Retrying { .. })
            {
                shared.set_status(Status::Unpaired);
            }
            pending = Some(commands.recv().await.unwrap_or(Command::Shutdown));
            continue;
        }

        shared.set_status(Status::Connecting);
        let (endpoint, first) = match &pair {
            Some((code, _)) => {
                (shared.endpoint.clone(), hello(&shared, Auth::Pair { pair: code.clone() }))
            }
            None => {
                let (device_id, token) = stored.credentials().expect("linked");
                let auth = Auth::Token { device_id: device_id.into(), token: token.into() };
                (
                    stored.endpoint.clone().unwrap_or_else(|| shared.endpoint.clone()),
                    hello(&shared, auth),
                )
            }
        };

        let attempt = tokio::select! {
            attempt = handshake(&shared, &endpoint, first) => attempt,
            command = commands.recv() => {
                pending = Some(command.unwrap_or(Command::Shutdown));
                continue;
            }
        };

        let failure = match attempt {
            Ok((sink, stream, welcome)) => {
                let reply = pair.take().map(|(_, reply)| reply);
                match link(&shared, &endpoint, &welcome, reply.is_some()) {
                    Ok(()) => {}
                    Err(e) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(PairError::Store(e)));
                        }
                        continue;
                    }
                }
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(Linked {
                        device_id: welcome.device_id.clone(),
                        member: welcome.member.clone(),
                    }));
                }
                let started = Instant::now();
                let served =
                    serve(&shared, &factory, sink, stream, &welcome, &mut commands, &mut pending)
                        .await;
                if pending.is_some() {
                    continue;
                }
                if started.elapsed() >= shared.backoff.max {
                    delay = shared.backoff.min;
                }
                served.err().unwrap_or_else(|| "the connection closed".into())
            }
            Err(Failure::Refused(refused)) => {
                let reply = pair.take().map(|(_, reply)| reply);
                let pairing = reply.is_some();
                if let Some(reply) = reply {
                    let error = match refused.reason {
                        RefusedReason::Code => PairError::Code(refused.message.clone()),
                        _ => PairError::Refused(refused.reason.clone(), refused.message.clone()),
                    };
                    let _ = reply.send(Err(error));
                }
                tracing::warn!("relay: Lumina refused: {:?}: {}", refused.reason, refused.message);
                match refused.reason {
                    // A bad code changes nothing about an existing link.
                    RefusedReason::Code if pairing => continue,
                    RefusedReason::Unlinked => {
                        forget_link(&shared);
                        shared.set_status(Status::Unlinked { message: refused.message });
                        continue;
                    }
                    RefusedReason::Version => {
                        parked = true;
                        shared.set_status(Status::Stopped { message: refused.message });
                        continue;
                    }
                    RefusedReason::Disabled => {
                        delay = shared.backoff.max;
                    }
                    _ => {}
                }
                if pairing {
                    continue;
                }
                format!("Lumina refused: {}", refused.message)
            }
            Err(Failure::Failed(error)) => {
                if let Some((_, reply)) = pair.take() {
                    // A member waiting on a code gets the error now, rather
                    // than a silent retry.
                    let _ = reply.send(Err(PairError::Connect(error)));
                    continue;
                }
                tracing::warn!("relay: could not reach Lumina: {error}");
                error
            }
        };

        let wait = jittered(delay);
        shared.set_status(Status::Retrying { after: wait, error: failure });
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            command = commands.recv() => pending = Some(command.unwrap_or(Command::Shutdown)),
        }
        delay = (delay * 2).min(shared.backoff.max);
    }
}

/// Store what a welcome says before serving anything. For a pairing, a
/// failed write fails the pairing. For a reconnect, the token just used stays
/// valid until the new one is presented, so a failed write is logged and the
/// next connect uses the old token.
fn link(shared: &Shared, endpoint: &str, welcome: &Welcome, pairing: bool) -> Result<(), String> {
    let mut stored = shared.lock_stored();
    let mut next = stored.clone();
    next.endpoint = Some(endpoint.to_owned());
    next.device_id = Some(welcome.device_id.clone());
    next.token = Some(welcome.token.clone());
    if welcome.member.is_some() || pairing {
        next.linked_to = welcome.member.clone();
    }
    match shared.store.save(&next) {
        Ok(()) => {
            *stored = next;
            Ok(())
        }
        Err(e) if pairing => Err(e.to_string()),
        Err(e) => {
            tracing::error!(
                "relay: could not store the new device token, keeping the old one: {e}"
            );
            Ok(())
        }
    }
}

fn forget_link(shared: &Shared) {
    let mut stored = shared.lock_stored();
    let mut next = stored.clone();
    next.forget_link();
    if let Err(e) = shared.store.save(&next) {
        tracing::error!("relay: could not clear the stored link: {e}");
    }
    *stored = next;
}

/// Serve MCP until the socket goes or a command arrives (left in
/// `pending`).
async fn serve<S, F>(
    shared: &Arc<Shared>,
    factory: &F,
    sink: FrameSink<Socket, AppFrame>,
    stream: FrameStream<Socket, LuminaFrame>,
    welcome: &Welcome,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    pending: &mut Option<Command>,
) -> Result<(), String>
where
    S: ServerHandler,
    F: Fn() -> S,
{
    let gated = Gated::new(factory(), shared.clone());
    let transport = (
        sink.retype::<rmcp::model::ServerJsonRpcMessage>(),
        stream.retype::<rmcp::model::ClientJsonRpcMessage>(),
    );
    let serving = tokio::select! {
        serving = rmcp::serve_server(gated, transport) => serving,
        command = commands.recv() => {
            *pending = Some(command.unwrap_or(Command::Shutdown));
            return Ok(());
        }
    };
    let running = serving.map_err(|e| format!("MCP did not start: {e}"))?;
    shared.set_status(Status::Connected {
        device_id: welcome.device_id.clone(),
        linked_to: shared.lock_stored().linked_to.clone(),
    });
    let token = running.cancellation_token();
    let result = tokio::select! {
        _ = running.waiting() => Ok(()),
        command = commands.recv() => {
            *pending = Some(command.unwrap_or(Command::Shutdown));
            token.cancel();
            Ok(())
        }
    };
    shared.set_peer(None);
    result
}

/// Tell Lumina to forget this device, then forget it here whatever Lumina
/// said: the member asked to disconnect.
async fn unlink(shared: &Shared) {
    let stored = shared.lock_stored().clone();
    if let Some((device_id, token)) = stored.credentials() {
        let endpoint = stored.endpoint.clone().unwrap_or_else(|| shared.endpoint.clone());
        let frame = AppFrame::Unlink(Unlink { device_id: device_id.into(), token: token.into() });
        match handshake(shared, &endpoint, frame).await {
            Err(Failure::Refused(r)) if r.reason == RefusedReason::Unlinked => {}
            Err(Failure::Failed(e)) => {
                tracing::warn!("relay: could not tell Lumina to unlink: {e}")
            }
            _ => tracing::warn!("relay: Lumina did not confirm the unlink"),
        }
    }
    forget_link(shared);
    shared.set_status(Status::Unpaired);
}
