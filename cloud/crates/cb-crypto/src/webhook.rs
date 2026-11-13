//! Signatures on outbound webhook calls.
//!
//! The server sends two headers beside the request body: a Unix timestamp
//! and `v1=` followed by the lowercase hex HMAC-SHA-256, keyed with the
//! signing secret, of the timestamp as decimal text, a `.` and the exact
//! body bytes. A consumer recomputes the MAC over what it received and
//! compares it in constant time, then checks the timestamp is recent.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Prefix of the signature header's value: the scheme version.
pub const WEBHOOK_SIGNATURE_PREFIX: &str = "v1=";

fn mac(secret: &[u8], timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The value of the signature header for `body` sent at `timestamp`.
pub fn webhook_signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    format!(
        "{WEBHOOK_SIGNATURE_PREFIX}{}",
        hex::encode(mac(secret, timestamp, body).finalize().into_bytes())
    )
}

/// True if `header` is the signature of `body` at `timestamp`. The
/// comparison takes constant time. This does not look at the timestamp's
/// age; the caller decides how old a call may be.
pub fn verify_webhook_signature(secret: &[u8], timestamp: i64, body: &[u8], header: &str) -> bool {
    let Some(hexpart) = header.strip_prefix(WEBHOOK_SIGNATURE_PREFIX) else {
        return false;
    };
    let Ok(expected) = hex::decode(hexpart) else {
        return false;
    };
    mac(secret, timestamp, body).verify_slice(&expected).is_ok()
}
