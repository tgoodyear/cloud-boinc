//! BOINC signatures.
//!
//! Every BOINC signature is the RSA private-key operation applied to an MD5
//! hex string (32 lowercase ASCII characters) with PKCS#1 v1.5 type 1 padding
//! and no `DigestInfo`; see upstream `encrypt_private`, `sign_file`,
//! `sign_block` and `generate_signature` in `lib/crypt.cpp`. It is carried as
//! a hex block (see [`crate::hexfmt`]).
//!
//! Three things are signed this way by a project:
//!
//! * **Application files.** `sign_executable` (`tools/sign_executable.cpp`)
//!   signs the MD5 of the file contents with the code signing private key;
//!   the block goes into `<file_signature>` of the file's `<file_info>`. The
//!   client checks it with `check_file_signature` against the project's
//!   `code_sign_key` (`FILE_INFO::verify_file` in `client/cs_files.cpp`). Use
//!   [`sign_md5`] / [`verify_md5`], or [`sign_bytes`] when the contents are at
//!   hand.
//! * **Upload certificates.** `add_signatures`
//!   (`tools/process_result_template.cpp`) signs, with the upload private
//!   key, the MD5 of the text `<name>NAME</name><max_nbytes>N</max_nbytes>`
//!   for each output file and appends it as `<xml_signature>` to the
//!   `<file_info>`. The upload handler (`handle_file_upload` in
//!   `sched/file_upload_handler.cpp`) rebuilds the same text from the
//!   `<name>` and `<max_nbytes>` the client sends back and calls
//!   `check_string_signature` with the upload public key. Use
//!   [`sign_upload_certificate`] / [`verify_upload_certificate`].
//! * **A replacement code signing key.** When a project changes its code
//!   signing key, the scheduler sends the new public key text as
//!   `<code_sign_key>` together with `<code_sign_key_signature>`: a signature,
//!   made with the *old* private key, of the new key text
//!   (`send_code_sign_key` in `sched/handle_request.cpp`). The client checks
//!   it with `check_string_signature` against the key it already holds
//!   (`CLIENT_STATE::handle_scheduler_reply` in `client/cs_scheduler.cpp`).
//!   Clients from version 7 on strip trailing whitespace from the key text
//!   first, so sign the key text with trailing whitespace removed
//!   (`signature_stripped_N` upstream). Use [`sign_text`] / [`verify_text`].
//!   The first key a client receives is not signed at all.

use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey};
use subtle::ConstantTimeEq;

use crate::error::Error;
use crate::hexfmt::{print_hex_data, scan_hex_data};
use crate::key::{PrivateKey, PublicKey};
use crate::md5::{is_md5_hex, md5_hex};

/// Overhead of PKCS#1 v1.5 padding: `00 01`, at least eight `ff`, `00`.
const PKCS1_OVERHEAD: usize = 11;

/// A private key prepared for signing.
///
/// Building a `Signer` converts and validates the key once; reuse it when
/// signing many items. The free functions [`sign_text`], [`sign_md5`] and
/// [`sign_bytes`] build one per call.
///
/// Signing uses no random number generator and therefore no blinding, which
/// keeps the default build free of an RNG dependency. Signatures are
/// deterministic.
#[derive(Clone)]
pub struct Signer {
    key: RsaPrivateKey,
}

impl Signer {
    /// Prepare a BOINC private key for signing.
    pub fn new(key: &PrivateKey) -> Result<Self, Error> {
        Ok(Self { key: key.to_rsa()? })
    }

    /// Use an existing [`RsaPrivateKey`].
    pub fn from_rsa(key: RsaPrivateKey) -> Self {
        Self { key }
    }

    /// The raw private-key operation of upstream `encrypt_private`:
    /// PKCS#1 v1.5 type 1 padding over `data`, no `DigestInfo`.
    ///
    /// As upstream, input longer than the key allows (modulus length minus
    /// 11 bytes) is truncated rather than rejected. MD5 hex strings always
    /// fit keys of 344 bits or more.
    pub fn encrypt_private(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let max_len = self.key.size().saturating_sub(PKCS1_OVERHEAD);
        let data = &data[..data.len().min(max_len)];
        Ok(self.key.sign(Pkcs1v15Sign::new_unprefixed(), data)?)
    }

    /// Sign an MD5 given as 32 lowercase hex characters and return the
    /// signature block. Equivalent to upstream `sign_file` followed by
    /// `sprint_hex_data`, for a file whose MD5 is already known.
    ///
    /// Anything other than 32 lowercase hex characters is refused: the
    /// verifier always compares against a lowercase digest, so a signature
    /// over, say, an uppercase digest could never verify.
    pub fn sign_md5(&self, md5_hex: &str) -> Result<String, Error> {
        if !is_md5_hex(md5_hex) {
            return Err(Error::InvalidMd5);
        }
        Ok(print_hex_data(&self.encrypt_private(md5_hex.as_bytes())?))
    }

    /// Sign arbitrary bytes (upstream `sign_block` + `sprint_hex_data`).
    pub fn sign_bytes(&self, data: &[u8]) -> Result<String, Error> {
        self.sign_md5(&md5_hex(data))
    }

    /// Sign a text (upstream `generate_signature`).
    pub fn sign_text(&self, text: &str) -> Result<String, Error> {
        self.sign_bytes(text.as_bytes())
    }

    /// Sign an upload certificate; see [`sign_upload_certificate`].
    pub fn sign_upload_certificate(&self, name: &str, max_nbytes: f64) -> Result<String, Error> {
        self.sign_text(&upload_certificate_text(name, max_nbytes))
    }
}

/// Sign a text and return the BOINC signature block (upstream
/// `generate_signature`): hex lines of 32 bytes, then `".\n"`.
///
/// Fails only if the key is not a usable RSA key.
pub fn sign_text(text: &str, key: &PrivateKey) -> Result<String, Error> {
    Signer::new(key)?.sign_text(text)
}

/// Sign arbitrary bytes (upstream `sign_block` + `sprint_hex_data`).
pub fn sign_bytes(data: &[u8], key: &PrivateKey) -> Result<String, Error> {
    Signer::new(key)?.sign_bytes(data)
}

/// Sign a file whose MD5 (32 lowercase hex characters) is already known;
/// see [`Signer::sign_md5`]. This is what `sign_executable` produces.
pub fn sign_md5(md5_hex: &str, key: &PrivateKey) -> Result<String, Error> {
    Signer::new(key)?.sign_md5(md5_hex)
}

/// The text an upload certificate signs, as built by upstream
/// `add_signatures` and rebuilt by `handle_file_upload`:
/// `<name>NAME</name><max_nbytes>N</max_nbytes>` with `N` printed as C
/// `%.0f`.
///
/// `name` is the file name as upstream `parse_str` returns it from the
/// `<file_info>`. `max_nbytes` must be finite: C and Rust print infinities
/// and NaN differently.
pub fn upload_certificate_text(name: &str, max_nbytes: f64) -> String {
    format!("<name>{name}</name><max_nbytes>{max_nbytes:.0}</max_nbytes>")
}

/// Sign an upload certificate for an output file. The result is the content
/// of the `<xml_signature>` element (upstream writes
/// `<xml_signature>\n%s</xml_signature>\n`).
pub fn sign_upload_certificate(
    name: &str,
    max_nbytes: f64,
    key: &PrivateKey,
) -> Result<String, Error> {
    Signer::new(key)?.sign_upload_certificate(name, max_nbytes)
}

/// The public-key operation of upstream `decrypt_public`: recover the data
/// that was signed, removing PKCS#1 v1.5 type 1 padding.
///
/// Follows OpenSSL's `EVP_PKEY_verify_recover` with `RSA_PKCS1_PADDING`: the
/// signature may be shorter than the modulus but not longer, must be less
/// than the modulus as a number, and the padding must have at least eight
/// `ff` bytes.
pub fn decrypt_public(key: &PublicKey, signature: &[u8]) -> Result<Vec<u8>, Error> {
    let n = key.n();
    let e = key.e();
    let k = n.bits().div_ceil(8);
    if k < PKCS1_OVERHEAD || e.bits() == 0 {
        return Err(Error::Crypto("unusable public key"));
    }
    if signature.len() > k {
        return Err(Error::Crypto("signature longer than modulus"));
    }
    let s = BigUint::from_bytes_be(signature);
    if s >= n {
        return Err(Error::Crypto("signature out of range"));
    }
    let m = s.modpow(&e, &n).to_bytes_be();
    let mut em = vec![0u8; k];
    em[k - m.len()..].copy_from_slice(&m);

    if em[0] != 0x00 || em[1] != 0x01 {
        return Err(Error::Crypto("bad padding"));
    }
    let ps_len = em[2..].iter().take_while(|&&b| b == 0xff).count();
    if ps_len < 8 || em.get(2 + ps_len) != Some(&0x00) {
        return Err(Error::Crypto("bad padding"));
    }
    Ok(em.split_off(2 + ps_len + 1))
}

/// Check a signature block against an MD5 hex string (upstream
/// `check_file_signature`, the form the client uses for application files).
///
/// `Ok(true)` / `Ok(false)` report a matching or non-matching signature.
/// `Err` reports what upstream returns as a non-zero status: no hex data in
/// the block ([`Error::BadHexFormat`]), or a block that is not a signature
/// made with this key's private half ([`Error::Crypto`]).
pub fn check_file_signature(
    md5_hex: &str,
    signature_block: &str,
    key: &PublicKey,
) -> Result<bool, Error> {
    let signature = scan_hex_data(signature_block);
    if signature.is_empty() {
        return Err(Error::BadHexFormat);
    }
    let decrypted = decrypt_public(key, &signature)?;
    if decrypted.is_empty() {
        return Err(Error::Crypto("empty signature payload"));
    }
    Ok(decrypted.ct_eq(md5_hex.as_bytes()).into())
}

/// Check a signature block against a text (upstream
/// `check_string_signature`). Errors as for [`check_file_signature`].
pub fn check_string_signature(
    text: &str,
    signature_block: &str,
    key: &PublicKey,
) -> Result<bool, Error> {
    check_file_signature(&md5_hex(text), signature_block, key)
}

/// True if `signature_block` is a valid BOINC signature of `text`.
pub fn verify_text(text: &str, signature_block: &str, key: &PublicKey) -> bool {
    check_string_signature(text, signature_block, key).unwrap_or(false)
}

/// True if `signature_block` is a valid BOINC signature of `data`.
pub fn verify_bytes(data: &[u8], signature_block: &str, key: &PublicKey) -> bool {
    check_file_signature(&md5_hex(data), signature_block, key).unwrap_or(false)
}

/// True if `signature_block` is a valid BOINC signature of the data whose
/// MD5 is `md5_hex` (32 lowercase hex characters).
pub fn verify_md5(md5_hex: &str, signature_block: &str, key: &PublicKey) -> bool {
    check_file_signature(md5_hex, signature_block, key).unwrap_or(false)
}

/// Check an upload certificate as upstream `handle_file_upload` does: rebuild
/// the signed text from the `<name>` and `<max_nbytes>` the client sent and
/// check `<xml_signature>` against the upload public key.
pub fn verify_upload_certificate(
    name: &str,
    max_nbytes: f64,
    xml_signature: &str,
    key: &PublicKey,
) -> bool {
    verify_text(
        &upload_certificate_text(name, max_nbytes),
        xml_signature,
        key,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_certificate_text_matches_printf() {
        // C: printf("<name>%s</name><max_nbytes>%.0f</max_nbytes>", ...)
        for (n, expect) in [
            (5_000_000.0, "5000000"),
            (0.0, "0"),
            (0.5, "0"),
            (1.5, "2"),
            (2.5, "2"),
            (1e20, "100000000000000000000"),
            (123456789012.7, "123456789013"),
        ] {
            assert_eq!(
                upload_certificate_text("f", n),
                format!("<name>f</name><max_nbytes>{expect}</max_nbytes>")
            );
        }
    }
}
