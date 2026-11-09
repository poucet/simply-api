//! The handshake frames, before MCP starts on the socket.
//!
//! The app opens `wss://<host>/devices/v1` and sends exactly one [`Hello`]
//! (or [`Unlink`]) as its first text frame. Lumina answers with exactly one
//! [`Welcome`] or [`Refused`]. After a `Welcome`, every frame in either
//! direction is one MCP JSON-RPC message, with Lumina as the MCP client; after
//! a `Refused`, Lumina closes the socket.
//!
//! The device token travels in the first frame, never in the URL, so it does
//! not reach access logs. Every `Welcome` carries a **fresh** token: the app
//! stores it before it serves anything, and the token it used stays valid on
//! Lumina's side until the new one is first presented (DEVICE-RELAY §3).
//!
//! ```text
//! app → {"type":"hello","v":1,"app":"tussen","version":"0.9.11","label":"studio-mac",
//!        "consent":{"read_only":false,"actions":{"session":false}},
//!        "pair":"K7Q4-9X2M-PLRT"}                       first link, or
//!        …"device_id":"d-…","token":"…"}                 every later connect
//! app → {"type":"unlink","device_id":"d-…","token":"…"}  "Disconnect" in the app
//! ← Lumina {"type":"welcome","device_id":"d-…","token":"…","member":"chris"}
//! ← Lumina {"type":"refused","reason":"code","message":"That code has expired."}
//! ```
//!
//! These types are plain serde and carry no rmcp type, so the listener can
//! use them as they are.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The protocol revision a [`Hello`] speaks. It is also the path segment:
/// `/devices/v1`.
pub const VERSION: u32 = 1;

/// Where a device dials by default.
pub const DEFAULT_ENDPOINT: &str = "wss://lumina.simplychris.ai/devices/v1";

/// The `_meta` key Lumina puts on every `tools/call`, saying why it asks
/// (`"bug tussen-0042"`, `"jam #jam-chris"`). The app writes it into its
/// call log.
pub const PURPOSE_META_KEY: &str = "lumina/purpose";

/// The app's first frame, in either direction of the protocol's life.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppFrame {
    Hello(Hello),
    Unlink(Unlink),
}

/// Lumina's one answer to an [`AppFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LuminaFrame {
    Welcome(Welcome),
    Refused(Refused),
}

/// "This is me, and this is what I let you ask."
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// [`VERSION`].
    pub v: u32,
    /// The app's name: `tussen`, `flux`.
    pub app: String,
    /// The app's version.
    pub version: String,
    /// The member's name for this copy of the app: `studio-mac`.
    pub label: String,
    /// The switches as they are now. Informational for Lumina (it shows them
    /// in `/support devices`); the app enforces them whatever Lumina thinks.
    pub consent: ConsentReport,
    /// How the app proves who it is.
    #[serde(flatten)]
    pub auth: Auth,
}

/// A pairing code, or a device id with its token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Auth {
    /// The code `/support link` showed the member.
    Pair { pair: String },
    /// A linked device.
    Token { device_id: String, token: String },
}

/// "Forget this device." Lumina answers `Refused` with reason `unlinked`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Unlink {
    pub device_id: String,
    pub token: String,
}

/// The switches, as reported in a [`Hello`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentReport {
    /// "Let Lumina run diagnostics."
    pub read_only: bool,
    /// One switch per action class, by class name (`session`).
    #[serde(default)]
    pub actions: BTreeMap<String, bool>,
}

/// "You're in." MCP starts with the next frame.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub device_id: String,
    /// The token for the next connect. Store it before serving.
    pub token: String,
    /// Whose device this is, as Lumina names the member, so the app can show
    /// "linked to @name".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

/// "No." Lumina closes the socket after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refused {
    pub reason: RefusedReason,
    /// One sentence for the member.
    #[serde(default)]
    pub message: String,
}

/// Why a hello was refused, which decides what the app does next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusedReason {
    /// The pairing code is wrong, used or expired. Nothing is stored; the
    /// app keeps whatever link it had.
    Code,
    /// The token is unknown or revoked, or the device was just unlinked. The
    /// app forgets its device id and token and stops dialling.
    Unlinked,
    /// Devices are switched off on Lumina's side (the owner's kill switch).
    /// The app keeps its link and retries at its slowest pace.
    Disabled,
    /// Too many attempts, or too many sockets. Retry with backoff.
    Limit,
    /// This protocol revision is no longer served. The app stops dialling
    /// until it is updated.
    Version,
    /// A reason this app does not know yet: treated as [`Limit`](Self::Limit).
    #[serde(other)]
    Other,
}

// Tokens and codes are secrets: never in a Debug line.
fn redacted(s: &str) -> &'static str {
    if s.is_empty() {
        ""
    } else {
        "[redacted]"
    }
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("v", &self.v)
            .field("app", &self.app)
            .field("version", &self.version)
            .field("label", &self.label)
            .field("consent", &self.consent)
            .field("auth", &self.auth)
            .finish()
    }
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pair { pair } => f.debug_struct("Pair").field("pair", &redacted(pair)).finish(),
            Self::Token { device_id, token } => f
                .debug_struct("Token")
                .field("device_id", device_id)
                .field("token", &redacted(token))
                .finish(),
        }
    }
}

impl std::fmt::Debug for Unlink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unlink")
            .field("device_id", &self.device_id)
            .field("token", &redacted(&self.token))
            .finish()
    }
}

impl std::fmt::Debug for Welcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Welcome")
            .field("device_id", &self.device_id)
            .field("token", &redacted(&self.token))
            .field("member", &self.member)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_read_as_documented() {
        let hello: AppFrame = serde_json::from_value(json!({
            "type": "hello", "v": 1, "app": "tussen", "version": "0.9.11", "label": "studio-mac",
            "consent": {"read_only": true, "actions": {"session": false}},
            "pair": "K7Q4-9X2M-PLRT"
        }))
        .expect("a pairing hello");
        let AppFrame::Hello(hello) = hello else { panic!("hello") };
        assert_eq!(hello.auth, Auth::Pair { pair: "K7Q4-9X2M-PLRT".into() });

        let token: Auth =
            serde_json::from_value(json!({"device_id": "d1", "token": "t1"})).expect("token");
        assert_eq!(token, Auth::Token { device_id: "d1".into(), token: "t1".into() });

        let refused: LuminaFrame = serde_json::from_value(json!({
            "type": "refused", "reason": "something_new", "message": "?"
        }))
        .expect("an unknown reason still reads");
        assert_eq!(
            refused,
            LuminaFrame::Refused(Refused { reason: RefusedReason::Other, message: "?".into() })
        );
    }

    #[test]
    fn debug_never_shows_a_token_or_code() {
        let welcome = Welcome { device_id: "d1".into(), token: "s3cret".into(), member: None };
        let pair = Auth::Pair { pair: "K7Q4".into() };
        let shown = format!("{welcome:?} {pair:?}");
        assert!(!shown.contains("s3cret") && !shown.contains("K7Q4"), "{shown}");
    }
}
