//! Account-related hashes and identifiers.
//!
//! All of these are MD5 hex strings (32 lowercase hex characters) upstream.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::md5::md5_hex;

/// The password hash a client sends: `md5(password + email_addr)`.
///
/// Upstream: `get_passwd_hash` in `lib/gui_rpc_client_ops.cpp` (the client
/// lower-cases the email address first, in `RPC_CLIENT::lookup_account`) and
/// `check_passwd_ui` in `html/inc/user_util.inc`. Pass the email address
/// already lower-cased.
pub fn passwd_hash(password: &str, email_lowercase: &str) -> String {
    let mut buf = Vec::with_capacity(password.len() + email_lowercase.len());
    buf.extend_from_slice(password.as_bytes());
    buf.extend_from_slice(email_lowercase.as_bytes());
    let hash = md5_hex(&buf);
    zeroize::Zeroize::zeroize(&mut buf);
    hash
}

/// A hard-to-guess string of 32 hex characters from 16 caller-supplied
/// random bytes, as `random_string` in `html/inc/util_basic.inc`
/// (`bin2hex(random_bytes(16))`).
///
/// Upstream `make_user` (`html/inc/user_util.inc`) uses this for a new
/// account's `authenticator` and for its `cross_project_id`. The bytes must
/// come from a cryptographically secure source, and each identifier needs
/// its own 16 bytes.
pub fn random_hex_string(random: &[u8; 16]) -> String {
    hex::encode(random)
}

/// The weak authenticator of an account:
/// `<userid>_<md5(authenticator + passwd_hash)>`.
///
/// Upstream: `weak_auth` in `html/inc/user.inc` and `get_weak_auth` in
/// `sched/sched_types.cpp`. `passwd_hash` is the value stored in the user
/// record, whatever its format, so the weak authenticator changes whenever
/// the stored hash does. A server must use the same stored value when
/// issuing and when checking.
pub fn weak_auth(user_id: u64, authenticator: &str, passwd_hash: &str) -> String {
    format!(
        "{user_id}_{}",
        md5_hex(format!("{authenticator}{passwd_hash}"))
    )
}

/// The RSS authenticator of an account:
/// `<userid>_<md5(authenticator + passwd_hash + "notify_rss")>`
/// (`get_rss_auth` in `sched/sched_types.cpp`).
pub fn rss_auth(user_id: u64, authenticator: &str, passwd_hash: &str) -> String {
    format!(
        "{user_id}_{}",
        md5_hex(format!("{authenticator}{passwd_hash}notify_rss"))
    )
}

/// The user ID a presented authenticator claims, if it has the form of a
/// weak authenticator.
///
/// Upstream `authenticate_user` (`sched/handle_request.cpp`) treats any
/// authenticator containing `_` as weak and reads the user ID from its
/// start. This returns `None` unless the part before the first `_` is a
/// plain decimal number, which is the only form that can pass
/// [`check_weak_auth`].
pub fn weak_auth_user_id(presented: &str) -> Option<u64> {
    let (id, _) = presented.split_once('_')?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if id.len() > 1 && id.starts_with('0') {
        return None;
    }
    id.parse().ok()
}

/// True if `presented` is the weak authenticator of the given account, as
/// `authenticate_user` in `sched/handle_request.cpp` checks it. The
/// comparison takes constant time.
pub fn check_weak_auth(
    presented: &str,
    user_id: u64,
    authenticator: &str,
    passwd_hash: &str,
) -> bool {
    constant_time_eq(
        presented.as_bytes(),
        weak_auth(user_id, authenticator, passwd_hash).as_bytes(),
    )
}

/// The email hash sent in scheduler replies: `md5(email_addr)`
/// (`authenticate_user` in `sched/handle_request.cpp`).
pub fn email_hash(email_addr: &str) -> String {
    md5_hex(email_addr)
}

/// The public cross-project ID of a user:
/// `md5(cross_project_id + email_addr)`.
///
/// Upstream: `external_cpid` in `SCHEDULER_REPLY::write`
/// (`sched/sched_types.cpp`) and `<cpid>` in `write_user`
/// (`sched/db_dump.cpp`). `cross_project_id` is the internal value stored in
/// the user record.
pub fn user_cpid(cross_project_id: &str, email_addr: &str) -> String {
    md5_hex(format!("{cross_project_id}{email_addr}"))
}

/// The host cross-project ID stored in the host record:
/// `md5(client_host_cpid + email_addr)`, where `client_host_cpid` is the
/// `<host_cpid>` the client reported.
///
/// Upstream: `modify_host_struct` and `find_host_by_cpid` in
/// `sched/handle_request.cpp`.
pub fn host_cpid(client_host_cpid: &str, email_addr: &str) -> String {
    md5_hex(format!("{client_host_cpid}{email_addr}"))
}

/// Compare two byte strings in constant time (lengths may leak).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Protect a `passwd_hash` for storage: `HMAC-SHA-256(pepper, passwd_hash)`
/// as 64 lowercase hex characters.
///
/// The MD5 `passwd_hash` is password-equivalent (a client logs in by sending
/// it), so storing it as-is lets anyone who reads the database log in.
/// Upstream wraps it with PHP `password_hash` (`do_passwd_rehash` in
/// `html/inc/user_util.inc`); this is the equivalent for a server that keeps
/// a secret pepper outside the database. The pepper should be at least 32
/// random bytes.
pub fn pepper_passwd_hash(pepper: &[u8], passwd_hash: &str) -> String {
    hex::encode(pepper_mac(pepper, passwd_hash).finalize().into_bytes())
}

/// True if `stored` (as returned by [`pepper_passwd_hash`]) matches the
/// presented `passwd_hash`. The comparison takes constant time.
pub fn verify_peppered_passwd_hash(pepper: &[u8], passwd_hash: &str, stored: &str) -> bool {
    let Ok(expected) = hex::decode(stored) else {
        return false;
    };
    pepper_mac(pepper, passwd_hash)
        .verify_slice(&expected)
        .is_ok()
}

/// The token a site form carries so that only a page the account holder
/// loaded can submit it (`form_tokens` in upstream `html/inc/util.inc`,
/// which uses `md5(time . authenticator)`): `HMAC-SHA-256(authenticator,
/// "form " + tnow)` as 64 lowercase hex characters. `tnow` is the time the
/// form was made, sent beside the token.
pub fn form_token(authenticator: &str, tnow: i64) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(authenticator.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(format!("form {tnow}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn pepper_mac(pepper: &[u8], passwd_hash: &str) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(pepper).expect("HMAC accepts keys of any length");
    mac.update(passwd_hash.as_bytes());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwd_hash_is_md5_of_concatenation() {
        // printf 'secretuser@example.com' | md5sum
        assert_eq!(
            passwd_hash("secret", "user@example.com"),
            md5_hex("secretuser@example.com")
        );
        assert_eq!(passwd_hash("abc", ""), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn random_string_is_hex_of_bytes() {
        let s = random_hex_string(&[
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0xff,
        ]);
        assert_eq!(s, "000102030405060708090a0b0c0d0eff");
    }

    #[test]
    fn weak_auth_round_trip() {
        let auth = "0123456789abcdef0123456789abcdef";
        let ph = passwd_hash("pw", "a@example.com");
        let weak = weak_auth(42, auth, &ph);
        assert_eq!(weak, format!("42_{}", md5_hex(format!("{auth}{ph}"))));
        assert_eq!(weak_auth_user_id(&weak), Some(42));
        assert!(check_weak_auth(&weak, 42, auth, &ph));
        assert!(!check_weak_auth(&weak, 43, auth, &ph));
        assert!(!check_weak_auth(&weak, 42, auth, "other"));
        assert!(!check_weak_auth(auth, 42, auth, &ph));
        assert_ne!(rss_auth(42, auth, &ph), weak);
    }

    #[test]
    fn weak_auth_user_id_forms() {
        assert_eq!(weak_auth_user_id("7_abc"), Some(7));
        assert_eq!(weak_auth_user_id("0_abc"), Some(0));
        assert_eq!(weak_auth_user_id("0123456789abcdef"), None);
        assert_eq!(weak_auth_user_id("_abc"), None);
        assert_eq!(weak_auth_user_id("07_abc"), None);
        assert_eq!(weak_auth_user_id("+7_abc"), None);
        assert_eq!(weak_auth_user_id("7x_abc"), None);
        assert_eq!(weak_auth_user_id("99999999999999999999999_abc"), None);
    }

    #[test]
    fn cpids() {
        assert_eq!(email_hash("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(user_cpid("ab", "c"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(host_cpid("a", "bc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn pepper() {
        // RFC 4231 test case 2
        assert_eq!(
            pepper_passwd_hash(b"Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let stored = pepper_passwd_hash(b"pepper", "hash");
        assert!(verify_peppered_passwd_hash(b"pepper", "hash", &stored));
        assert!(!verify_peppered_passwd_hash(b"pepper", "hash2", &stored));
        assert!(!verify_peppered_passwd_hash(b"other", "hash", &stored));
        assert!(!verify_peppered_passwd_hash(b"pepper", "hash", "not hex"));
        assert!(!verify_peppered_passwd_hash(
            b"pepper",
            "hash",
            &stored[..62]
        ));
    }

    #[test]
    fn form_tokens_depend_on_key_and_time() {
        let t = form_token("abc", 1000);
        assert_eq!(t.len(), 64);
        assert_eq!(t, form_token("abc", 1000));
        assert_ne!(t, form_token("abc", 1001));
        assert_ne!(t, form_token("abd", 1000));
        assert_eq!(
            t,
            pepper_passwd_hash(b"abc", "form 1000"),
            "the same HMAC as the pepper, keyed by the account key"
        );
    }
}
