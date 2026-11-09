//! The signature: `sha256=<hex HMAC-SHA256(secret, "<timestamp>.<raw body>")>`.
//!
//! The timestamp is the `X-Simply-Timestamp` header's value as sent (unix
//! seconds, decimal), and the body is the exact bytes of the request body.
//! A receiver recomputes it over what it received and compares in constant
//! time ([`verify`]); it should also refuse a timestamp too far from its own
//! clock, which is what stops a captured request being replayed later.

use ring::hmac;

/// The header names, as sent.
pub const EVENT_HEADER: &str = "X-Simply-Event";
pub const DELIVERY_HEADER: &str = "X-Simply-Delivery";
pub const TIMESTAMP_HEADER: &str = "X-Simply-Timestamp";
pub const SIGNATURE_HEADER: &str = "X-Simply-Signature";

/// The signature header's prefix.
pub const PREFIX: &str = "sha256=";

/// `X-Simply-Signature`'s value for `body` sent at `timestamp` (unix
/// seconds) under `secret`.
pub fn sign(secret: &[u8], timestamp: u64, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(timestamp.to_string().as_bytes());
    ctx.update(b".");
    ctx.update(body);
    format!("{PREFIX}{}", hex(ctx.sign().as_ref()))
}

/// Does `signature` (the header's value) sign `body` at `timestamp` under
/// `secret`? Compared in constant time.
pub fn verify(secret: &[u8], timestamp: u64, body: &[u8], signature: &str) -> bool {
    let Some(hex_sig) = signature.strip_prefix(PREFIX) else { return false };
    let Some(tag) = unhex(hex_sig) else { return false };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut msg = Vec::with_capacity(body.len() + 21);
    msg.extend_from_slice(timestamp.to_string().as_bytes());
    msg.push(b'.');
    msg.extend_from_slice(body);
    hmac::verify(&key, &msg, &tag).is_ok()
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes().chunks(2).map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector in the crate docs: other repos pin the same one.
    #[test]
    fn the_documented_vector_signs_and_verifies() {
        let body = br#"{"event":"jam.started","at":"2023-11-14T22:13:20Z","data":{"participants":2,"session":"3f2a9c"}}"#;
        let sig = sign(b"whsec_test_123", 1700000000, body);
        assert_eq!(sig, "sha256=0f9a8cf6653bad9a5bcf9b884970aa7e37933e85c2316e33c681533688a003f4");
        assert!(verify(b"whsec_test_123", 1700000000, body, &sig));
        assert!(verify(
            b"whsec_test_123",
            1700000000,
            body,
            &sig.to_uppercase().replace("SHA256=", "sha256=")
        ));
        assert!(!verify(b"whsec_test_124", 1700000000, body, &sig), "another secret");
        assert!(!verify(b"whsec_test_123", 1700000001, body, &sig), "another timestamp");
        assert!(!verify(b"whsec_test_123", 1700000000, &body[1..], &sig), "another body");
        assert!(!verify(b"whsec_test_123", 1700000000, body, &sig[7..]), "no prefix");
        assert!(!verify(b"whsec_test_123", 1700000000, body, "sha256=zz"), "not hex");
    }
}
