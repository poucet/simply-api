//! **simply-relay**: an app dials out to Lumina and serves its MCP tools over
//! that connection, behind the member's consent.
//!
//! The app opens one WebSocket to Lumina (`wss://…/devices/v1`), proves who it
//! is in the first frame ([`wire`]), and then serves any rmcp 3.x
//! [`ServerHandler`] over the socket, one JSON-RPC
//! message per text frame ([`frames`]), with Lumina as the MCP client.
//! Nothing on the member's side listens, so NAT and firewalls don't matter.
//!
//! Between Lumina and the app's handler sits the [`gate`]: every tool is off
//! until the member switches its class on, and a refused call never reaches
//! the handler. Every call is written to the member's own call log
//! ([`calls`]). The standard diagnostics ([`diag`]) are served beside the
//! app's tools.
//!
//! # Adopting it
//!
//! An app compiles the relay in behind its own optional feature, so a build
//! without it carries none of this:
//!
//! ```toml
//! [features]
//! lumina = ["dep:simply-relay"]
//! [dependencies]
//! simply-relay = { git = "https://github.com/poucet/simply-api", tag = "v0.2.0", optional = true }
//! ```
//!
//! ```rust,ignore
//! #[cfg(feature = "lumina")]
//! let relay = simply_relay::Relay::builder("tussen", env!("CARGO_PKG_VERSION"), config_dir)
//!     .read_only(["get_state", "levels", "mesh_peers"])
//!     .config(move || serde_json::to_value(hub.config()).unwrap_or_default())
//!     .redactor(|config| { config["studio"]["owner_email"] = "[redacted]".into(); })
//!     .spawn(move || McpApiServer::new(api.clone(), tussen_api_tool_router::<dyn TussenApi>()))?;
//! // once, where the app builds its tracing subscriber:
//! #[cfg(feature = "lumina")]
//! let subscriber = subscriber.with(simply_relay::log_layer());
//! ```
//!
//! The settings panel then calls [`Relay::pair`] with the code `/support
//! link` showed, [`Relay::set_read_only`] / [`Relay::set_action`] for the
//! switches, [`Relay::recent_calls`] for the log and [`Relay::disconnect`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::service::{Peer, RoleServer};
use rmcp::ServerHandler;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

pub mod calls;
pub mod diag;
mod dial;
pub mod frames;
pub mod gate;
pub mod store;
pub mod wire;

pub use calls::{CallRecord, Verdict};
pub use diag::log_layer;
pub use gate::Gated;
pub use store::{Class, Consent};

/// Where the relay is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not linked: waiting for [`Relay::pair`].
    Unpaired,
    /// Dialling, or saying hello.
    Connecting,
    /// Serving Lumina.
    Connected { device_id: String, linked_to: Option<String> },
    /// The last attempt failed; the next one is after this long.
    Retrying { after: Duration, error: String },
    /// Lumina no longer knows this device (unlinked or revoked). The link is
    /// forgotten; pair again to come back.
    Unlinked { message: String },
    /// Lumina no longer serves this app's protocol revision. The app needs an
    /// update; nothing is dialled until then.
    Stopped { message: String },
}

/// What a successful [`Relay::pair`] linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    pub device_id: String,
    /// Whose account, as Lumina named the member: show "linked to @name".
    pub member: Option<String>,
}

/// Why [`Relay::pair`] did not link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The code is wrong, already used, or expired. Lumina's sentence.
    Code(String),
    /// Lumina refused for another reason (devices switched off, too many
    /// attempts…).
    Refused(wire::RefusedReason, String),
    /// Lumina could not be reached, or did not answer in time.
    Connect(String),
    /// Linked, but the token could not be written, so the link is dropped.
    Store(String),
    /// Another pairing or a shutdown replaced this one.
    Interrupted,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(m) => {
                write!(f, "{}", if m.is_empty() { "that code did not work" } else { m })
            }
            Self::Refused(reason, m) => write!(f, "Lumina refused ({reason:?}): {m}"),
            Self::Connect(m) => write!(f, "could not reach Lumina: {m}"),
            Self::Store(m) => write!(f, "could not save the link: {m}"),
            Self::Interrupted => write!(f, "pairing was interrupted"),
        }
    }
}

impl std::error::Error for PairError {}

/// Reconnect pacing: start at `min`, double on each failure up to `max`,
/// each wait jittered by ±25 %.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { min: Duration::from_secs(1), max: Duration::from_secs(60) }
    }
}

type ConfigSource = Arc<dyn Fn() -> Value + Send + Sync>;

/// State every connection and the app's handle share.
pub(crate) struct Shared {
    pub app: String,
    pub version: String,
    pub label: String,
    pub endpoint: String,
    /// The app's declared tools and their classes.
    pub classes: BTreeMap<String, Class>,
    pub action_classes: Vec<String>,
    pub store: store::Store,
    pub stored: Mutex<store::Stored>,
    pub calls: calls::CallLog,
    pub about: diag::AboutSource,
    pub config: Option<ConfigSource>,
    pub redactor: Option<diag::Redactor>,
    pub backoff: Backoff,
    pub liveness: Duration,
    pub handshake_timeout: Duration,
    pub peer: Mutex<Option<Peer<RoleServer>>>,
    pub status: watch::Sender<Status>,
    pub runtime: tokio::runtime::Handle,
}

impl Shared {
    pub fn consent(&self) -> Consent {
        self.lock_stored().consent.clone()
    }

    pub fn lock_stored(&self) -> std::sync::MutexGuard<'_, store::Stored> {
        self.stored.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The app's config, through its redactor and then [`diag::redact`].
    pub fn config(&self) -> Value {
        let Some(source) = &self.config else { return Value::Null };
        let mut config = source();
        if let Some(redactor) = &self.redactor {
            redactor(&mut config);
        }
        diag::redact(&mut config);
        config
    }

    pub fn set_peer(&self, peer: Option<Peer<RoleServer>>) {
        *self.peer.lock().unwrap_or_else(|p| p.into_inner()) = peer;
    }

    /// Tell a connected Lumina to list the tools again.
    fn notify_list_changed(&self) {
        let peer = self.peer.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(peer) = peer {
            self.runtime.spawn(async move {
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), peer.notify_tool_list_changed())
                        .await;
            });
        }
    }

    pub fn set_status(&self, status: Status) {
        self.status.send_replace(status);
    }
}

/// Builds a [`Relay`]; see the crate docs.
pub struct Builder {
    app: String,
    version: String,
    dir: PathBuf,
    log_dir: Option<PathBuf>,
    endpoint: String,
    label: Option<String>,
    build: Option<String>,
    classes: BTreeMap<String, Class>,
    action_classes: Vec<String>,
    config: Option<ConfigSource>,
    redactor: Option<diag::Redactor>,
    backoff: Backoff,
    liveness: Duration,
    handshake_timeout: Duration,
}

impl Builder {
    /// Dial somewhere other than [`wire::DEFAULT_ENDPOINT`]. A linked device
    /// keeps dialling the endpoint that issued its token.
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    /// The member's name for this copy of the app (`studio-mac`). Defaults
    /// to the machine's host name.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// A build id `lumina_about` reports (a git or jj change id).
    pub fn build(mut self, build: impl Into<String>) -> Self {
        self.build = Some(build.into());
        self
    }

    /// Where the call log goes (default: the directory the token file is in).
    pub fn log_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.log_dir = Some(dir.into());
        self
    }

    /// Declare these of the app's tools read-only: the diagnostics switch
    /// covers them, and `lumina_report` runs the ones that take no argument.
    /// Written by hand on purpose: nothing is read-only by inference.
    pub fn read_only<I: IntoIterator<Item = T>, T: Into<String>>(mut self, tools: I) -> Self {
        for tool in tools {
            self.classes.insert(tool.into(), Class::ReadOnly);
        }
        self
    }

    /// Declare these of the app's tools as actions of `class`, which gets its
    /// own switch (`session`: "Let Lumina open and join jam sessions").
    ///
    /// # Panics
    /// If `class` is `read_only`, which is the diagnostics switch's name.
    pub fn action<I: IntoIterator<Item = T>, T: Into<String>>(
        mut self,
        class: impl Into<String>,
        tools: I,
    ) -> Self {
        let class = class.into();
        assert_ne!(class, "read_only", "`read_only` is the diagnostics class, not an action class");
        for tool in tools {
            self.classes.insert(tool.into(), Class::Action(class.clone()));
        }
        if !self.action_classes.contains(&class) {
            self.action_classes.push(class);
        }
        self
    }

    /// The app's config for `lumina_config`, as JSON (any `Serialize` config
    /// converts with `serde_json::to_value`). Called on each request.
    pub fn config(mut self, source: impl Fn() -> Value + Send + Sync + 'static) -> Self {
        self.config = Some(Arc::new(source));
        self
    }

    /// The app's own redactor, run on the config before the standard one
    /// ([`diag::redact`]) blanks secret-looking keys: for what only the app
    /// knows is private.
    pub fn redactor(mut self, redactor: impl Fn(&mut Value) + Send + Sync + 'static) -> Self {
        self.redactor = Some(Arc::new(redactor));
        self
    }

    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// How long a silent socket is kept (default 90 s; Lumina pings every
    /// 30 s).
    pub fn liveness(mut self, liveness: Duration) -> Self {
        self.liveness = liveness;
        self
    }

    /// How long Lumina gets to answer the hello (default 10 s).
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Start the relay on the current tokio runtime. `factory` builds the
    /// app's handler afresh for each connection, as a streamable-HTTP
    /// service does per session.
    ///
    /// Fails if the stored file exists but cannot be read. Must be called
    /// from within a tokio runtime.
    pub fn spawn<S, F>(self, factory: F) -> std::io::Result<Relay>
    where
        S: ServerHandler,
        F: Fn() -> S + Send + Sync + 'static,
    {
        let store = store::Store::new(&self.dir);
        let mut stored = store.load()?;
        for class in &self.action_classes {
            stored.consent.actions.entry(class.clone()).or_insert(false);
        }
        let status =
            if stored.credentials().is_some() { Status::Connecting } else { Status::Unpaired };
        let (status, _) = watch::channel(status);
        let label = self.label.unwrap_or_else(|| host_name().unwrap_or_else(|| self.app.clone()));
        let shared = Arc::new(Shared {
            about: diag::AboutSource {
                app: self.app.clone(),
                version: self.version.clone(),
                build: self.build,
                started: std::time::Instant::now(),
            },
            app: self.app,
            version: self.version,
            label,
            endpoint: self.endpoint,
            classes: self.classes,
            action_classes: self.action_classes,
            calls: calls::CallLog::new(self.log_dir.as_deref().unwrap_or(&self.dir)),
            store,
            stored: Mutex::new(stored),
            config: self.config,
            redactor: self.redactor,
            backoff: self.backoff,
            liveness: self.liveness,
            handshake_timeout: self.handshake_timeout,
            peer: Mutex::new(None),
            status,
            runtime: tokio::runtime::Handle::current(),
        });
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = shared.runtime.spawn(dial::run(shared.clone(), factory, receiver));
        Ok(Relay { shared, commands, task: Arc::new(Mutex::new(Some(task))) })
    }
}

fn host_name() -> Option<String> {
    let from_env = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).ok();
    let name = from_env.or_else(|| {
        let out = std::process::Command::new("hostname").output().ok()?;
        String::from_utf8(out.stdout).ok()
    })?;
    let name = name.trim().trim_end_matches(".local").to_owned();
    (!name.is_empty()).then_some(name)
}

pub(crate) enum Command {
    Pair(String, oneshot::Sender<Result<Linked, PairError>>),
    Unlink(oneshot::Sender<()>),
    Shutdown,
}

/// The app's handle on its relay. Cheap to clone; every clone is the same
/// relay.
#[derive(Clone)]
pub struct Relay {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Relay {
    /// Start building a relay for `app` at `version`, keeping its file in
    /// `dir` (the app's config directory).
    pub fn builder(
        app: impl Into<String>,
        version: impl Into<String>,
        dir: impl AsRef<Path>,
    ) -> Builder {
        Builder {
            app: app.into(),
            version: version.into(),
            dir: dir.as_ref().to_owned(),
            log_dir: None,
            endpoint: wire::DEFAULT_ENDPOINT.into(),
            label: None,
            build: None,
            classes: BTreeMap::new(),
            action_classes: Vec::new(),
            config: None,
            redactor: None,
            backoff: Backoff::default(),
            liveness: Duration::from_secs(90),
            handshake_timeout: Duration::from_secs(10),
        }
    }

    pub fn status(&self) -> Status {
        self.shared.status.borrow().clone()
    }

    /// Follow status changes (for a settings panel).
    pub fn watch_status(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }

    /// Link this app with the code `/support link` showed the member. A
    /// linked app is re-linked (to whoever's code it is). Returns once Lumina
    /// has answered and the token is stored; the relay then serves on that
    /// connection.
    pub async fn pair(&self, code: &str) -> Result<Linked, PairError> {
        let (tx, rx) = oneshot::channel();
        let code = code.trim().to_owned();
        if self.commands.send(Command::Pair(code, tx)).is_err() {
            return Err(PairError::Interrupted);
        }
        rx.await.unwrap_or(Err(PairError::Interrupted))
    }

    /// "Disconnect": tell Lumina to forget this device (best effort, if it
    /// can be reached) and forget the link here. The switches stay as they
    /// are.
    pub async fn disconnect(&self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Unlink(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// The switches.
    pub fn consent(&self) -> Consent {
        self.shared.consent()
    }

    /// Flip "Let Lumina run diagnostics". Takes effect at once: stored, and a
    /// connected Lumina is told to list the tools again.
    pub fn set_read_only(&self, on: bool) -> std::io::Result<()> {
        self.update_consent(|c| c.read_only = on)
    }

    /// Flip an action class's switch. Errors on a class the app did not
    /// declare with [`Builder::action`].
    pub fn set_action(&self, class: &str, on: bool) -> std::io::Result<()> {
        if !self.shared.action_classes.iter().any(|c| c == class) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{class} is not an action class this app declared"),
            ));
        }
        self.update_consent(|c| {
            c.actions.insert(class.to_owned(), on);
        })
    }

    fn update_consent(&self, change: impl FnOnce(&mut Consent)) -> std::io::Result<()> {
        {
            let mut stored = self.shared.lock_stored();
            let mut next = stored.clone();
            change(&mut next.consent);
            if next == *stored {
                return Ok(());
            }
            self.shared.store.save(&next)?;
            *stored = next;
        }
        self.shared.notify_list_changed();
        Ok(())
    }

    /// The last `n` calls Lumina made, oldest first (the settings panel shows
    /// 50).
    pub fn recent_calls(&self, n: usize) -> Vec<CallRecord> {
        self.shared.calls.recent(n)
    }

    /// The token file.
    pub fn store_path(&self) -> &Path {
        self.shared.store.path()
    }

    /// The call log.
    pub fn call_log_path(&self) -> &Path {
        self.shared.calls.path()
    }

    /// Close the connection and stop dialling.
    pub async fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}
