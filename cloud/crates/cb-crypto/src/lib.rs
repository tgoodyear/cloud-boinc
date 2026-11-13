//! BOINC cryptography in pure Rust.
//!
//! This crate re-implements the parts of the BOINC C++ library that a project
//! server needs, in a form that builds for native targets and for
//! `wasm32-unknown-unknown`:
//!
//! * [`key`]: the text key file format written by `print_key_hex` and read by
//!   `scan_key_hex` in upstream `lib/crypt.cpp`, with conversions to and from
//!   the [`rsa`] crate's key types.
//! * [`hexfmt`]: the hex block format of `sprint_hex_data` / `sscan_hex_data`.
//! * [`sign`]: signatures as produced by `sign_file`, `sign_block` and
//!   `generate_signature`, and checks as done by `check_file_signature` and
//!   `check_string_signature`.
//! * [`md5`]: MD5 helpers matching `md5_block` / `md5_file`.
//! * [`account`]: password hashes, authenticators, weak authenticators and
//!   cross-project IDs.
//! * `keygen` (cargo feature `keygen`): 1024-bit key pair generation as
//!   `crypt_prog -genkey` does.
//!
//! # What a BOINC signature is
//!
//! A BOINC signature is not a standard "RSA with MD5" signature. The signer
//! computes the MD5 of the data, renders it as 32 lowercase hex characters,
//! and applies the RSA private-key operation to those 32 ASCII bytes with
//! PKCS#1 v1.5 type 1 padding and no `DigestInfo` (upstream `encrypt_private`
//! calls `EVP_PKEY_sign` with `RSA_PKCS1_PADDING` and no digest set). The
//! verifier recovers the 32 bytes with the public key (`decrypt_public`) and
//! compares them with the MD5 hex string it computed itself. The signature is
//! transported as a hex block: 32 bytes per line, then a line holding `.`.

pub mod account;
mod error;
pub mod hexfmt;
pub mod key;
#[cfg(feature = "keygen")]
pub mod keygen;
pub mod md5;
pub mod sign;
pub mod webhook;

pub use account::{
    check_weak_auth, constant_time_eq, email_hash, host_cpid, passwd_hash, pepper_passwd_hash,
    random_hex_string, rss_auth, user_cpid, verify_peppered_passwd_hash, weak_auth,
    weak_auth_user_id,
};
pub use error::Error;
pub use hexfmt::{print_hex_data, scan_hex_data};
pub use key::{
    PrivateKey, PublicKey, MAX_RSA_MODULUS_BITS, MAX_RSA_MODULUS_LEN, MAX_RSA_PRIME_BITS,
    MAX_RSA_PRIME_LEN,
};
#[cfg(feature = "keygen")]
pub use keygen::{generate_key_pair, generate_key_pair_with_rng};
pub use md5::{is_md5_hex, md5_hex, Md5Stream, MD5_HEX_LEN};
pub use sign::{
    check_file_signature, check_string_signature, decrypt_public, sign_bytes, sign_md5, sign_text,
    sign_upload_certificate, upload_certificate_text, verify_bytes, verify_md5, verify_text,
    verify_upload_certificate, Signer,
};
pub use webhook::{verify_webhook_signature, webhook_signature, WEBHOOK_SIGNATURE_PREFIX};
