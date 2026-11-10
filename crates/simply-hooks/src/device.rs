//! **Linked devices**: an app install linked to its member's account at a
//! receiver (Lumina's `/<app> link`) signs as that member. The receiver
//! swaps a one-time code for a **device id** and a **key**; from then on the
//! install signs exactly as the `simply` scheme does ([`crate::sign`]),
//! with the key as the secret, plus [`DEVICE_HEADER`] naming the device.
//! Deliveries go through [`crate::Target::device`], so every attempt,
//! retries from the outbox included, carries it.
//!
//! # The calls
//!
//! - [`link`]: `POST <base>/devices/v1/link`, body `{code, app, label?,
//!   version?}` → `200 {device_id, key, member: {id, name}, server}`.
//!   `401 {"error":"code not valid"}` for a code unknown, expired, used or
//!   another app's; `403` the member left; `409` too many devices; `429`
//!   with `Retry-After`.
//! - [`unlink`]: `POST <base>/devices/v1/unlink`, body `{}`, signed as the
//!   device: `200` whether or not it was still linked.
//!
//! Both block the calling thread for one request (a runtime of their own),
//! so call them from a thread that may wait, never from inside a tokio
//! runtime. The key is in exactly one answer, and is never logged.
//!
//! **Test vector** (Lumina's): signing
//! `{"event":"jam.shared","at":"2026-11-11T20:00:00Z","data":{"title":"Late-night ambient"}}`
//! at `X-Timestamp: 1794427200` with the key
//! `3167aacb872b76d2ce93f55c1ef7a589517f37241fc74357e59ff4c7a921ba4b`
//! gives `X-Signature: sha256=d496a56434d659e13bff2a2ede2dbd1d44db23cfe5ca01b2716fb12604843487`.
//!
//! ```
//! let body = br#"{"event":"jam.shared","at":"2026-11-11T20:00:00Z","data":{"title":"Late-night ambient"}}"#;
//! let key = "3167aacb872b76d2ce93f55c1ef7a589517f37241fc74357e59ff4c7a921ba4b";
//! assert_eq!(
//!     simply_hooks::sign::sign(key.as_bytes(), 1794427200, body),
//!     "sha256=d496a56434d659e13bff2a2ede2dbd1d44db23cfe5ca01b2716fb12604843487",
//! );
//! ```

use std::time::Duration;

use crate::http::{self, Endpoint};
use crate::{sign, Secret};

/// The header naming the device a request is signed by.
pub const DEVICE_HEADER: &str = "X-Hook-Device";
/// A link code's length, without its dashes.
pub const CODE_LEN: usize = 12;
/// The characters a code is made of (Crockford base32).
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// What an app sends to link: the code the member was shown, the app it
/// was issued for, and what the member will see the install as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRequest {
    pub code: String,
    /// The app id (`tussen`): a code issued for another app is refused.
    pub app: String,
    /// What the member sees this install as (at most 40 characters; empty:
    /// none).
    pub label: String,
    /// The app's build string (at most 40 characters).
    pub version: String,
}

/// A link made: the device id and key to sign with, and whose they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    /// 32 lowercase hex digits.
    pub device_id: String,
    /// 64 hex digits; its `Debug` never shows it.
    pub key: Secret,
    /// The member's id at the receiver (Discord's, as a string).
    pub member_id: String,
    pub member_name: String,
    /// The server the code came from.
    pub server: String,
}

/// Why a link or an unlink didn't happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// Not a code (checked before anything is sent).
    NotACode,
    /// The receiver answered `status`: its `{"error"}`, and its
    /// `Retry-After` seconds.
    Refused { status: u16, error: Option<String>, retry_after: Option<u64> },
    /// No answer: the network, a timeout, a URL refused.
    Unreachable(String),
    /// An answer that isn't a link.
    BadAnswer(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NotACode => write!(f, "not a link code ({CODE_LEN} letters and digits)"),
            LinkError::Refused { status, error, .. } => match error {
                Some(e) => write!(f, "refused: HTTP {status} ({e})"),
                None => write!(f, "refused: HTTP {status}"),
            },
            LinkError::Unreachable(e) => write!(f, "no answer: {e}"),
            LinkError::BadAnswer(e) => write!(f, "not a link: {e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// A code as typed or pasted, back to its 12 characters, read as the
/// receiver reads it: any case, dashes and spaces dropped, `I`/`L` as 1,
/// `O` as 0. `None`: not a code.
pub fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::new();
    for ch in input.trim().chars().filter(|c| !matches!(c, '-' | ' ')) {
        let ch = match ch.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        };
        if !ch.is_ascii() || !CROCKFORD.contains(&(ch as u8)) {
            return None;
        }
        out.push(ch);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// `n` lowercase hex digits?
fn hex_of(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The headers that sign `body` at `timestamp` as `device_id` with `key`:
/// [`DEVICE_HEADER`], `X-Timestamp`, `X-Signature`.
pub fn signature_headers(
    device_id: &str,
    key: &Secret,
    timestamp: u64,
    body: &[u8],
) -> Vec<(String, String)> {
    vec![
        (DEVICE_HEADER.to_owned(), device_id.to_owned()),
        (sign::TIMESTAMP_HEADER.to_owned(), timestamp.to_string()),
        (sign::SIGNATURE_HEADER.to_owned(), sign::sign(key.expose().as_bytes(), timestamp, body)),
    ]
}

/// One POST to `<base><path>`, on a runtime of its own.
fn call(
    base: &str,
    path: &str,
    mut headers: Vec<(String, String)>,
    body: &str,
    user_agent: &str,
    timeout: Duration,
) -> Result<http::Answer, LinkError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let endpoint = Endpoint::parse(&url).map_err(LinkError::Unreachable)?;
    headers.push(("User-Agent".to_owned(), user_agent.to_owned()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LinkError::Unreachable(e.to_string()))?;
    rt.block_on(http::post(&endpoint, &headers, body.as_bytes(), timeout))
        .map_err(LinkError::Unreachable)
}

fn refused(a: &http::Answer) -> LinkError {
    let error = serde_json::from_slice::<serde_json::Value>(&a.body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned));
    LinkError::Refused { status: a.code, error, retry_after: a.retry_after() }
}

/// Swap `req.code` at the receiver at `base` (`https://lumina.example`;
/// `http://` only to loopback) for a device id and key. Blocks for the
/// one request, at most `timeout`.
pub fn link(base: &str, req: &LinkRequest, timeout: Duration) -> Result<Linked, LinkError> {
    let code = normalize_code(&req.code).ok_or(LinkError::NotACode)?;
    let short = |s: &str| s.trim().chars().take(40).collect::<String>();
    let mut body = serde_json::json!({ "code": code, "app": req.app });
    if !req.label.trim().is_empty() {
        body["label"] = short(&req.label).into();
    }
    if !req.version.trim().is_empty() {
        body["version"] = short(&req.version).into();
    }
    let agent = format!("{}/{}", req.app, req.version);
    let a = call(base, "/devices/v1/link", vec![], &body.to_string(), &agent, timeout)?;
    if a.code != 200 {
        return Err(refused(&a));
    }
    let v: serde_json::Value = serde_json::from_slice(&a.body)
        .map_err(|_| LinkError::BadAnswer("the answer isn't JSON".into()))?;
    let bad = |what: &str| LinkError::BadAnswer(format!("the answer has no {what}"));
    let device_id =
        v["device_id"].as_str().filter(|d| hex_of(d, 32)).ok_or_else(|| bad("device id"))?;
    let key = v["key"].as_str().filter(|k| hex_of(k, 64)).ok_or_else(|| bad("key"))?;
    let member = &v["member"];
    let member_id = member["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| member["id"].as_u64().map(|n| n.to_string()))
        .ok_or_else(|| bad("member"))?;
    Ok(Linked {
        device_id: device_id.to_owned(),
        key: Secret::new(key),
        member_id,
        member_name: member["name"].as_str().unwrap_or_default().to_owned(),
        server: v["server"].as_str().unwrap_or_default().to_owned(),
    })
}

/// Tell the receiver at `base` to forget `device_id`: a signed `{}`.
/// `Ok` once it answers 200 (it does whether or not the device was still
/// linked). `user_agent` as the app's other requests send it.
pub fn unlink(
    base: &str,
    device_id: &str,
    key: &Secret,
    user_agent: &str,
    timeout: Duration,
) -> Result<(), LinkError> {
    let body = "{}";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut headers = signature_headers(device_id, key, now, body.as_bytes());
    headers.push((sign::DELIVERY_HEADER.to_owned(), uuid::Uuid::new_v4().to_string()));
    let a = call(base, "/devices/v1/unlink", headers, body, user_agent, timeout)?;
    match a.code {
        200 => Ok(()),
        _ => Err(refused(&a)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_read_as_the_receiver_reads_them() {
        assert_eq!(normalize_code("K7Q4-9X2M-PLRT").as_deref(), Some("K7Q49X2MP1RT"));
        assert_eq!(normalize_code(" k7q4 9x2m plrt ").as_deref(), Some("K7Q49X2MP1RT"));
        assert_eq!(normalize_code("O0Il-0000-0000").as_deref(), Some("001100000000"));
        assert_eq!(normalize_code("K7Q4-9X2M"), None, "too short");
        assert_eq!(normalize_code("K7Q4-9X2M-PLRU"), None, "U is no Crockford digit");
        assert_eq!(normalize_code("K7Q4-9X2M-PLRÉ"), None);
    }

    /// Lumina's device-hook vector.
    #[test]
    fn a_device_signs_as_lumina_checks_it() {
        let key = Secret::new("3167aacb872b76d2ce93f55c1ef7a589517f37241fc74357e59ff4c7a921ba4b");
        let body = br#"{"event":"jam.shared","at":"2026-11-11T20:00:00Z","data":{"title":"Late-night ambient"}}"#;
        let h = signature_headers("0123456789abcdef0123456789abcdef", &key, 1794427200, body);
        assert_eq!(
            h,
            [
                ("X-Hook-Device".to_owned(), "0123456789abcdef0123456789abcdef".to_owned()),
                ("X-Timestamp".to_owned(), "1794427200".to_owned()),
                (
                    "X-Signature".to_owned(),
                    "sha256=d496a56434d659e13bff2a2ede2dbd1d44db23cfe5ca01b2716fb12604843487"
                        .to_owned()
                ),
            ]
        );
        assert!(!format!(
            "{:?}",
            Linked {
                device_id: "d".into(),
                key,
                member_id: "1".into(),
                member_name: "a".into(),
                server: "s".into()
            }
        )
        .contains("3167aacb"));
    }
}
