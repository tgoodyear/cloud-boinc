//! Signature headers on the validator webhook call.

use cb_core::{webhook_headers, WEBHOOK_SIGNATURE_HEADER as SIG, WEBHOOK_TIMESTAMP_HEADER as TS};
use cb_crypto::{verify_webhook_signature, webhook_signature};

const BODY: &str = "{\"call_id\":\"wu1-rev2-check_set\"}";

fn get<'a>(h: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
}

#[test]
fn known_answer() {
    // HMAC-SHA-256 over "<timestamp>.<body>", computed outside this code base.
    assert_eq!(
        webhook_signature(b"test-signing-secret", 1_700_000_000, BODY.as_bytes()),
        "v1=6123c14d3be499c6871e7d5fa278ece23a7aa646b58ba9adbd8ea2fcb31636a0"
    );
}

#[test]
fn headers_are_present_and_verify_with_the_secret() {
    let h = webhook_headers(Some("tok"), Some("sek"), 1_700_000_000, BODY);
    assert_eq!(get(&h, "Authorization"), Some("Bearer tok"));
    assert_eq!(get(&h, TS), Some("1700000000"));
    let sig = get(&h, SIG).unwrap();
    assert!(sig.starts_with("v1="));
    assert!(verify_webhook_signature(
        b"sek",
        1_700_000_000,
        BODY.as_bytes(),
        sig
    ));
    // The bearer token is not the signing key.
    assert!(!verify_webhook_signature(
        b"tok",
        1_700_000_000,
        BODY.as_bytes(),
        sig
    ));
}

#[test]
fn without_a_signing_secret_the_call_is_as_before() {
    let h = webhook_headers(Some("tok"), None, 1_700_000_000, BODY);
    assert_eq!(h, vec![("Authorization", "Bearer tok".to_string())]);
    assert!(webhook_headers(None, None, 1_700_000_000, BODY).is_empty());
}

#[test]
fn a_signing_secret_alone_sends_no_bearer_token() {
    let h = webhook_headers(None, Some("sek"), 5, BODY);
    assert_eq!(get(&h, "Authorization"), None);
    assert!(get(&h, TS).is_some() && get(&h, SIG).is_some());
}

#[test]
fn the_signature_covers_the_exact_bytes_and_the_timestamp() {
    let h = webhook_headers(None, Some("sek"), 100, BODY);
    let sig = get(&h, SIG).unwrap();
    assert!(verify_webhook_signature(b"sek", 100, BODY.as_bytes(), sig));
    // Same JSON, different bytes (whitespace): the signature does not hold.
    let spaced = BODY.replace(':', ": ");
    assert!(!verify_webhook_signature(
        b"sek",
        100,
        spaced.as_bytes(),
        sig
    ));
    // A one-byte change, another timestamp, another secret.
    assert!(!verify_webhook_signature(b"sek", 100, b"{}", sig));
    assert!(!verify_webhook_signature(b"sek", 101, BODY.as_bytes(), sig));
    assert!(!verify_webhook_signature(b"sex", 100, BODY.as_bytes(), sig));
    // The timestamp is part of the signed text, so it cannot be moved into
    // the body: "1" + "." + "0.x" differs from "10" + "." + "x".
    assert_ne!(
        webhook_signature(b"sek", 1, b"0.x"),
        webhook_signature(b"sek", 10, b"x")
    );
}

#[test]
fn malformed_signature_headers_do_not_verify() {
    for bad in ["", "v1=", "v1=zz", "v2=00", "00", "sha256=00"] {
        assert!(!verify_webhook_signature(b"sek", 1, BODY.as_bytes(), bad));
    }
}
