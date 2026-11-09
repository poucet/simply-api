//! The standard read-only diagnostics every app serves (DEVICE-RELAY §5.3).
//!
//! | Tool | Returns |
//! |---|---|
//! | `lumina_about` | app, version, build, OS and version, arch, uptime, locale, CPU count |
//! | `lumina_config` | the app's config after the app's redactor **and** [`redact`] |
//! | `lumina_errors` | the last 50 warnings and errors [`log_layer`] kept |
//! | `lumina_log` | the last N (at most 500) log lines |
//! | `lumina_report` | all of the above, plus each read-only app tool with no required argument, run once |
//!
//! Log lines and config values have the member's home directory shown as
//! `~`. The ring is process-wide, filled by the [`log_layer`] the app adds to
//! its `tracing` subscriber once; without it, `lumina_errors` and
//! `lumina_log` are empty.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use rmcp::model::{JsonObject, Tool};
use serde_json::{json, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;

/// How many log lines the ring keeps.
pub const LOG_LINES: usize = 500;
/// How many warnings and errors the ring keeps.
pub const ERROR_LINES: usize = 50;
/// How many log lines `lumina_report` includes.
pub const REPORT_LOG_LINES: usize = 100;

pub const ABOUT: &str = "lumina_about";
pub const CONFIG: &str = "lumina_config";
pub const ERRORS: &str = "lumina_errors";
pub const LOG: &str = "lumina_log";
pub const REPORT: &str = "lumina_report";

/// Every standard tool's name.
pub const TOOLS: [&str; 5] = [ABOUT, CONFIG, ERRORS, LOG, REPORT];

// ── the log ring ────────────────────────────────────────────────────────

/// One kept log line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogLine {
    pub at: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

#[derive(Default)]
struct Ring {
    lines: VecDeque<LogLine>,
    errors: VecDeque<LogLine>,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(Mutex::default);

fn push(line: LogLine, is_error: bool) {
    let mut ring = RING.lock().unwrap_or_else(|p| p.into_inner());
    if is_error {
        if ring.errors.len() == ERROR_LINES {
            ring.errors.pop_front();
        }
        ring.errors.push_back(line.clone());
    }
    if ring.lines.len() == LOG_LINES {
        ring.lines.pop_front();
    }
    ring.lines.push_back(line);
}

/// The last `n` log lines, oldest first, home paths scrubbed.
pub fn recent_log(n: usize) -> Vec<LogLine> {
    let ring = RING.lock().unwrap_or_else(|p| p.into_inner());
    let skip = ring.lines.len().saturating_sub(n);
    ring.lines.iter().skip(skip).cloned().map(scrub_line).collect()
}

/// The kept warnings and errors, oldest first, home paths scrubbed.
pub fn recent_errors() -> Vec<LogLine> {
    let ring = RING.lock().unwrap_or_else(|p| p.into_inner());
    ring.errors.iter().cloned().map(scrub_line).collect()
}

fn scrub_line(mut line: LogLine) -> LogLine {
    line.message = scrub_home(&line.message);
    line
}

/// A `tracing` layer that keeps the last [`LOG_LINES`] lines and the last
/// [`ERROR_LINES`] warnings and errors for the diagnostics tools. Add it once
/// to the app's subscriber.
pub fn log_layer() -> LogLayer {
    LogLayer
}

/// See [`log_layer`].
pub struct LogLayer;

impl<S: Subscriber> tracing_subscriber::Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = Message::default();
        event.record(&mut visitor);
        let level = *event.metadata().level();
        push(
            LogLine {
                at: crate::calls::now_rfc3339(),
                level: level.to_string(),
                target: event.metadata().target().to_owned(),
                message: visitor.0,
            },
            level <= Level::WARN,
        );
    }
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(value);
        } else {
            self.record_debug(field, &value);
        }
    }
}

// ── redaction ───────────────────────────────────────────────────────────

/// What a redacted value is replaced with.
pub const REDACTED: &str = "[redacted]";

/// The app's own redactor, run on its config before [`redact`].
pub type Redactor = Arc<dyn Fn(&mut Value) + Send + Sync>;

/// Blank every value whose key looks like a secret (a word of the key is
/// `token`, `secret`, `password`, `passwd`, `auth`, `authorization`,
/// `credential(s)`, `cookie`, `key`, `apikey` or `privatekey`, or ends in `token` or
/// `secret`),
/// every value under a `secrets` table, and show the home directory as `~`
/// in every string left.
pub fn redact(value: &mut Value) {
    redact_in(value, false);
}

fn redact_in(value: &mut Value, secret: bool) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let hide = secret || secret_key(key);
                if hide && !v.is_object() && !v.is_array() {
                    *v = Value::String(REDACTED.into());
                } else {
                    redact_in(v, hide);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                if secret && !v.is_object() && !v.is_array() {
                    *v = Value::String(REDACTED.into());
                } else {
                    redact_in(v, secret);
                }
            }
        }
        Value::String(s) if secret => *s = REDACTED.into(),
        Value::String(s) => *s = scrub_home(s),
        _ if secret => *value = Value::String(REDACTED.into()),
        _ => {}
    }
}

/// Whether a config key names a secret.
pub fn secret_key(key: &str) -> bool {
    const WORDS: [&str; 13] = [
        "token",
        "secret",
        "secrets",
        "password",
        "passwd",
        "auth",
        "authorization",
        "credential",
        "credentials",
        "cookie",
        "key",
        "apikey",
        "privatekey",
    ];
    const ENDINGS: [&str; 2] = ["token", "secret"];
    words(key).iter().any(|w| WORDS.contains(&w.as_str()) || ENDINGS.iter().any(|e| w.ends_with(e)))
}

/// `apiKey`, `api_key`, `api-key` and `API.KEY` all split into `api`, `key`.
fn words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// `text` with the home directory shown as `~`.
pub fn scrub_home(text: &str) -> String {
    match home() {
        Some(home) if !home.is_empty() && home != "/" => text.replace(home.as_str(), "~"),
        _ => text.to_owned(),
    }
}

fn home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())
}

// ── about ───────────────────────────────────────────────────────────────

/// What `lumina_about` says about the app and the machine.
pub(crate) struct AboutSource {
    pub app: String,
    pub version: String,
    pub build: Option<String>,
    pub started: std::time::Instant,
}

impl AboutSource {
    pub fn about(&self) -> Value {
        json!({
            "app": self.app,
            "version": self.version,
            "build": self.build,
            "os": std::env::consts::OS,
            "os_version": os_version(),
            "arch": std::env::consts::ARCH,
            "uptime_secs": self.started.elapsed().as_secs(),
            "locale": std::env::var("LC_ALL").ok()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("LANG").ok()),
            "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        })
    }
}

fn os_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sw_vers").arg("-productVersion").output().ok()?;
        let v = String::from_utf8(out.stdout).ok()?.trim().to_owned();
        (!v.is_empty()).then(|| format!("macOS {v}"))
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/os-release").ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_owned())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

// ── tool specs ──────────────────────────────────────────────────────────

fn schema(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("a tool schema is an object"),
    }
}

/// The standard tools' specs.
pub fn tools() -> Vec<Tool> {
    let none = || schema(json!({ "type": "object", "properties": {} }));
    let mut log = Tool::new(
        LOG,
        "The app's most recent log lines, oldest first, with the home directory shown as ~.",
        schema(json!({
            "type": "object",
            "properties": {
                "lines": {
                    "type": "integer", "minimum": 1, "maximum": LOG_LINES,
                    "description": "How many lines (default 100, at most 500)."
                }
            }
        })),
    );
    log = log.annotate(rmcp::model::ToolAnnotations::new().read_only(true));
    let ro = |name: &'static str, description: &'static str| {
        Tool::new(name, description, none())
            .annotate(rmcp::model::ToolAnnotations::new().read_only(true))
    };
    vec![
        ro(ABOUT, "The app's name, version and build, and the machine's OS, architecture, uptime, locale and CPU count."),
        ro(CONFIG, "The app's configuration, with secrets redacted and the home directory shown as ~."),
        ro(ERRORS, "The app's last 50 warnings and errors, oldest first."),
        log,
        ro(REPORT, "Everything at once: about, config, errors, recent log, and each of the app's read-only checks that takes no argument, run once. Start here."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_are_recognised_and_lookalikes_are_not() {
        for key in [
            "token",
            "api_key",
            "apiKey",
            "API-KEY",
            "client_secret",
            "password",
            "auth",
            "Authorization",
            "accessToken",
            "private.key",
            "credentials",
            "session_cookie",
        ] {
            assert!(secret_key(key), "{key} is a secret");
        }
        for key in ["keyboard", "author", "monkey_count", "name", "sample_rate", "keys_layout"] {
            assert!(!secret_key(key), "{key} is not a secret");
        }
    }
}
