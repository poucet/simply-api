//! One JSON message per WebSocket text frame, as a `(Sink, Stream)` pair.
//!
//! rmcp's `transport::sink_stream` turns any `(Sink<Tx>, Stream<Rx>)` of
//! JSON-RPC messages into a transport, so this is all a WebSocket needs:
//! serialize each outgoing message into one text frame, parse each incoming
//! text frame as one message. The pair is generic over the message types, so
//! the same code carries the relay's handshake frames before MCP starts.
//!
//! Incoming frames that are not text (binary, a stray pong) are skipped;
//! pings are answered by tungstenite itself. A text frame that does not parse
//! as `Rx` is logged and skipped, as the MCP spec has a peer ignore a message
//! it cannot read rather than drop the session. A socket that has been silent
//! for longer than the liveness limit ends the stream, which ends the MCP
//! session: Lumina pings every 30 s, so silence means the path is dead.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{Sink, Stream, StreamExt};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, Sleep};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

/// What can go wrong writing a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The message did not serialize (a bug in the sender).
    Encode(serde_json::Error),
    /// The socket refused the frame or is closed.
    Socket(tungstenite::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "frame did not encode: {e}"),
            Self::Socket(e) => write!(f, "websocket: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// The writing half: each `Tx` becomes one text frame.
pub struct FrameSink<S, Tx> {
    inner: SplitSink<WebSocketStream<S>, Message>,
    _tx: PhantomData<fn(Tx)>,
}

/// The reading half: each text frame becomes one `Rx`.
pub struct FrameStream<S, Rx> {
    inner: SplitStream<WebSocketStream<S>>,
    liveness: Option<Duration>,
    silence: Option<Pin<Box<Sleep>>>,
    _rx: PhantomData<fn() -> Rx>,
}

/// Split `ws` into a JSON frame sink and stream. With `liveness`, the
/// stream ends once nothing at all (not even a ping) has arrived for that
/// long.
pub fn json_frames<S, Tx, Rx>(
    ws: WebSocketStream<S>,
    liveness: Option<Duration>,
) -> (FrameSink<S, Tx>, FrameStream<S, Rx>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (sink, stream) = ws.split();
    (
        FrameSink { inner: sink, _tx: PhantomData },
        FrameStream {
            inner: stream,
            liveness,
            silence: liveness.map(|d| Box::pin(tokio::time::sleep(d))),
            _rx: PhantomData,
        },
    )
}

impl<S, Tx> FrameSink<S, Tx> {
    /// Hand back the halves to re-type them: the relay sends handshake
    /// frames first and MCP messages after, over the same socket.
    pub fn retype<Tx2>(self) -> FrameSink<S, Tx2> {
        FrameSink { inner: self.inner, _tx: PhantomData }
    }
}

impl<S, Rx> FrameStream<S, Rx> {
    /// See [`FrameSink::retype`].
    pub fn retype<Rx2>(self) -> FrameStream<S, Rx2> {
        FrameStream {
            inner: self.inner,
            liveness: self.liveness,
            silence: self.silence,
            _rx: PhantomData,
        }
    }
}

impl<S, Tx> Sink<Tx> for FrameSink<S, Tx>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Tx: Serialize,
{
    type Error = FrameError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx).map_err(FrameError::Socket)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Tx) -> Result<(), Self::Error> {
        let text = serde_json::to_string(&item).map_err(FrameError::Encode)?;
        Pin::new(&mut self.inner).start_send(Message::text(text)).map_err(FrameError::Socket)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(FrameError::Socket)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(FrameError::Socket)
    }
}

impl<S, Rx> Stream for FrameStream<S, Rx>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Rx: DeserializeOwned,
{
    type Item = Rx;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Rx>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(message))) => {
                    if let (Some(limit), Some(silence)) = (self.liveness, self.silence.as_mut()) {
                        silence.as_mut().reset(Instant::now() + limit);
                    }
                    match message {
                        Message::Text(text) => match serde_json::from_str::<Rx>(&text) {
                            Ok(rx) => return Poll::Ready(Some(rx)),
                            Err(e) => tracing::warn!("relay: skipped an unreadable frame: {e}"),
                        },
                        Message::Close(_) => return Poll::Ready(None),
                        _ => {}
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::debug!("relay: socket read ended: {e}");
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {
                    if let Some(silence) = self.silence.as_mut() {
                        if silence.as_mut().poll(cx).is_ready() {
                            tracing::debug!("relay: socket silent past the liveness limit");
                            return Poll::Ready(None);
                        }
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}
