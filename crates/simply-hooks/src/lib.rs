//! **simply-hooks**: signed outbound webhooks. An app calls [`Hooks::emit`];
//! a background worker delivers the event to every target that wants it,
//! retrying from a small outbox kept on disk. The crate knows nothing about
//! any receiver: it is the same scheme whoever listens.
//!
//! # The wire scheme
//!
//! Every delivery is one `POST` with a JSON body:
//!
//! ```json
//! { "event": "<name>", "at": "<RFC 3339, UTC>", "data": { … } }
//! ```
//!
//! and these headers:
//!
//! | Header | Value |
//! |---|---|
//! | `Content-Type` | `application/json` |
//! | `X-Signature` | `sha256=` + lowercase hex HMAC-SHA256(secret, `"<timestamp>.<raw body>"`) |
//! | `X-Timestamp` | when this attempt was signed, unix seconds |
//! | `X-Hook-Delivery` | a UUID v4, the same on every attempt of one delivery |
//! | `X-Hook-Event` | the event's name, as in the body (a receiver may ignore it) |
//!
//! This is the scheme Lumina's hooks listener verifies and simply.audio
//! sends.
//!
//! A retry sends the same body and delivery id with a fresh timestamp and
//! signature, so a receiver deduplicates on `X-Hook-Delivery` and refuses
//! a timestamp far from its own clock. `at` is when the event happened, not
//! when it was sent.
//!
//! **The test vector** (pin it in any implementation, sender or receiver):
//!
//! - secret: `whsec_test_123`
//! - timestamp: `1700000000`
//! - body: `{"event":"jam.started","at":"2023-11-14T22:13:20Z","data":{"participants":2,"session":"3f2a9c"}}`
//! - `X-Signature`: `sha256=0f9a8cf6653bad9a5bcf9b884970aa7e37933e85c2316e33c681533688a003f4`
//!
//! ```
//! let body = br#"{"event":"jam.started","at":"2023-11-14T22:13:20Z","data":{"participants":2,"session":"3f2a9c"}}"#;
//! assert_eq!(
//!     simply_hooks::sign::sign(b"whsec_test_123", 1700000000, body),
//!     "sha256=0f9a8cf6653bad9a5bcf9b884970aa7e37933e85c2316e33c681533688a003f4",
//! );
//! ```
//!
//! # Delivery
//!
//! - A `2xx` answer delivers it. `408`, `425`, `429`, `5xx`, a timeout or a
//!   network error is retried with backoff ([`Retry`]), or after the answer's
//!   `Retry-After` seconds when that is later, until the delivery is
//!   older than [`Retry::max_age`]. Any other answer (a wrong secret's `401`,
//!   a `404`, a redirect) is refused for good: retrying would get the same.
//! - A body over [`Config::max_body`] (256 KiB by default, what Lumina
//!   takes) is refused at `emit`, logged, and never queued.
//! - `https://` only, but for a loopback host (`http://localhost:…`).
//!   Redirects are not followed and the answer's body is never read.
//! - Failures are logged with the event, the delivery id and the target's
//!   host: never the body, the URL's path or the secret.
//! - The outbox ([`Config::outbox`]) holds every delivery not yet made,
//!   survives a restart, and loses a delivery once it is made, refused or
//!   too old. It never holds a secret.
//!
//! # Adopting it
//!
//! ```rust,no_run
//! use simply_hooks::{Config, Hooks, Target};
//! let hooks = Hooks::new(
//!     Config::new(Some("/path/to/app-data/hooks-outbox.json".into()), "my-app/1.0")
//!         .target(Target::new("https://example.com/hooks", "secret").events(["jam.*", "app.started"])),
//! )?;
//! hooks.emit("app.started", serde_json::json!({ "version": "1.0" })); // returns at once
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Settings that change while the app runs go to [`Hooks::set_targets`];
//! a UI reads [`Hooks::status`].
//!
//! # Room for a pull mode
//!
//! An app behind a firewall that wants work *back* (long-polling a URL and
//! acting on the answer) is not built. The push API is shaped so it can be
//! added beside it: [`Config`] is `#[non_exhaustive]` and built by methods,
//! so a `pull` setting is one more method; the signing ([`sign`]) and the
//! envelope are public, so a poll request is signed the same way; and the
//! worker owns its runtime, so a poll loop is one more task in it.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::sync::mpsc;

mod http;
mod outbox;
pub mod sign;
mod worker;

/// A target's secret. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The secret itself, to sign with.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Where events go: a URL, the secret its signatures use, and which events
/// it wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    pub secret: Secret,
    /// `None`: every event. Otherwise each name exactly, or a prefix ending
    /// in `*` (`"jam.*"`).
    pub events: Option<Vec<String>>,
}

impl Target {
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> Target {
        Target { url: url.into(), secret: Secret::new(secret), events: None }
    }

    /// Only these events (exact names, or prefixes ending in `*`).
    pub fn events<I: IntoIterator<Item = S>, S: Into<String>>(mut self, events: I) -> Target {
        self.events = Some(events.into_iter().map(Into::into).collect());
        self
    }

    /// Does it want `event`?
    pub fn allows(&self, event: &str) -> bool {
        self.events.as_ref().is_none_or(|list| {
            list.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => event.starts_with(prefix),
                None => p == event,
            })
        })
    }
}

/// When a failed delivery is tried again: `first` after the first failure,
/// doubling up to `max`, until the delivery is older than `max_age`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub first: Duration,
    pub max: Duration,
    pub max_age: Duration,
}

impl Default for Retry {
    /// 10 s, doubling to 10 minutes, for a day.
    fn default() -> Retry {
        Retry {
            first: Duration::from_secs(10),
            max: Duration::from_secs(600),
            max_age: Duration::from_secs(24 * 3600),
        }
    }
}

impl Retry {
    /// The wait after attempt `attempts` (1: the first) failed.
    pub fn after(&self, attempts: u32) -> Duration {
        let factor = 1u32.checked_shl(attempts.saturating_sub(1)).unwrap_or(u32::MAX);
        self.first.saturating_mul(factor).min(self.max)
    }
}

/// How [`Hooks`] runs. Built by [`Config::new`] and its methods.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    pub targets: Vec<Target>,
    /// The outbox file (`None`: in memory only, lost when the app quits).
    pub outbox: Option<PathBuf>,
    pub retry: Retry,
    /// One attempt's limit, from connect to the answer's status line.
    pub timeout: Duration,
    /// The most deliveries kept; past it the oldest are dropped.
    pub max_pending: usize,
    /// `User-Agent`, e.g. `tussen/0.10.2`.
    pub user_agent: String,
    /// The largest body `emit` queues, in bytes.
    pub max_body: usize,
}

impl Config {
    /// No targets yet; a 10 s timeout, [`Retry::default`], 1000 deliveries
    /// at most, bodies up to 256 KiB.
    pub fn new(outbox: Option<PathBuf>, user_agent: impl Into<String>) -> Config {
        Config {
            targets: vec![],
            outbox,
            retry: Retry::default(),
            timeout: Duration::from_secs(10),
            max_pending: 1000,
            user_agent: user_agent.into(),
            max_body: 256 * 1024,
        }
    }

    pub fn target(mut self, t: Target) -> Config {
        self.targets.push(t);
        self
    }

    pub fn targets(mut self, t: Vec<Target>) -> Config {
        self.targets = t;
        self
    }

    pub fn retry(mut self, r: Retry) -> Config {
        self.retry = r;
        self
    }

    pub fn timeout(mut self, t: Duration) -> Config {
        self.timeout = t;
        self
    }

    pub fn max_body(mut self, bytes: usize) -> Config {
        self.max_body = bytes;
        self
    }

    pub fn max_pending(mut self, n: usize) -> Config {
        self.max_pending = n.max(1);
        self
    }
}

/// What a settings UI shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Deliveries in the outbox, every target's.
    pub pending: usize,
    pub targets: Vec<TargetStatus>,
}

/// One target's deliveries since the app started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetStatus {
    pub url: String,
    /// In the outbox now.
    pub pending: usize,
    pub delivered: u64,
    /// Refused for good, or given up on as too old.
    pub dropped: u64,
    pub last_ok: Option<SystemTime>,
    /// Why the last attempt failed (cleared by a delivery), or why the
    /// target is not used (a bad URL).
    pub last_error: Option<String>,
    pub last_error_at: Option<SystemTime>,
}

/// The envelope every body is.
#[derive(Serialize)]
struct Envelope<'a> {
    event: &'a str,
    at: String,
    data: serde_json::Value,
}

/// Is `event` a name this crate sends: `[a-z0-9._-]`, 1 to 64 long.
pub fn valid_event(event: &str) -> bool {
    (1..=64).contains(&event.len())
        && event
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// The handle: cheap to clone, every clone feeding the one worker. The
/// worker stops once every handle is dropped; what it had not delivered
/// stays in the outbox for the next start.
#[derive(Clone)]
pub struct Hooks {
    tx: mpsc::UnboundedSender<worker::Msg>,
    shared: Arc<worker::Shared>,
    max_body: usize,
}

impl Hooks {
    /// Start the worker, on a thread of its own: the app needs no async
    /// runtime. Fails only if the thread or its runtime can't start.
    pub fn new(config: Config) -> std::io::Result<Hooks> {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(worker::Shared::default());
        let max_body = config.max_body;
        worker::spawn(config, shared.clone(), rx)?;
        Ok(Hooks { tx, shared, max_body })
    }

    /// Queue `event` with `data` for every target that wants it, and return
    /// at once. A name [`valid_event`] refuses, data that isn't JSON, or a
    /// body over [`Config::max_body`] is logged and dropped.
    pub fn emit(&self, event: &str, data: impl Serialize) {
        if !valid_event(event) {
            tracing::warn!("webhooks: {event:?} is not an event name: not sent");
            return;
        }
        let data = match serde_json::to_value(data) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("webhooks: {event}'s data is not JSON ({e}): not sent");
                return;
            }
        };
        let now = chrono::Utc::now();
        let at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let body =
            serde_json::to_string(&Envelope { event, at, data }).expect("a Value serializes");
        if body.len() > self.max_body {
            tracing::warn!(
                "webhooks: {event} is {} bytes, over {}: not sent",
                body.len(),
                self.max_body
            );
            return;
        }
        let created = now.timestamp().max(0) as u64;
        self.send(worker::Msg::Emit { event: event.to_owned(), body, created });
    }

    /// Replace the targets. Deliveries pending for a URL no longer among
    /// them are dropped; an empty list turns sending off.
    pub fn set_targets(&self, targets: Vec<Target>) {
        self.send(worker::Msg::Targets(targets));
    }

    /// The outbox and each target, as of the worker's last step.
    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Wait, at most `limit`, until nothing is queued or pending: whether it
    /// got there. For a quitting app that wants its last event out, and
    /// tests; it blocks, so never call it where `emit` is called.
    pub fn wait_idle(&self, limit: Duration) -> bool {
        let until = std::time::Instant::now() + limit;
        loop {
            if self.shared.queued.load(Ordering::SeqCst) == 0 && self.status().pending == 0 {
                return true;
            }
            if std::time::Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&self, msg: worker::Msg) {
        self.shared.queued.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(msg).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("webhooks: the worker has stopped: not sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_takes_the_events_it_lists() {
        let all = Target::new("https://x", "s");
        assert!(all.allows("anything"));
        let some = Target::new("https://x", "s").events(["jam.*", "app.started"]);
        assert!(
            some.allows("jam.started") && some.allows("jam.ended") && some.allows("app.started")
        );
        assert!(!some.allows("app.stopped") && !some.allows("jams"));
        assert!(!Target::new("https://x", "s").events(Vec::<String>::new()).allows("jam.started"));
    }

    #[test]
    fn backoff_doubles_to_its_ceiling() {
        let r = Retry::default();
        assert_eq!(r.after(1), Duration::from_secs(10));
        assert_eq!(r.after(2), Duration::from_secs(20));
        assert_eq!(r.after(7), Duration::from_secs(600));
        assert_eq!(r.after(200), Duration::from_secs(600));
    }

    #[test]
    fn secrets_never_print() {
        let t = Target::new("https://x", "hunter2");
        assert!(!format!("{t:?}").contains("hunter2"));
    }

    #[test]
    fn event_names() {
        assert!(valid_event("jam.started") && valid_event("app_2-x"));
        assert!(
            !valid_event("")
                && !valid_event("Jam")
                && !valid_event("a b")
                && !valid_event(&"a".repeat(65))
        );
    }
}
