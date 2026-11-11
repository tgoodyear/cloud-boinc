//! Tests against fixtures produced by the upstream BOINC `crypt_prog`.
//! See `tests/data/README.txt` for how they were made. The key pair is a
//! throwaway, test-only key.

use cb_crypto::{
    check_file_signature, check_string_signature, md5_hex, print_hex_data, scan_hex_data,
    sign_bytes, sign_md5, sign_text, sign_upload_certificate, verify_bytes, verify_md5,
    verify_text, verify_upload_certificate, Error, Md5Stream, PrivateKey, PublicKey, Signer,
};

const PRIVATE_KEY: &str = include_str!("data/TEST_ONLY_private_key");
const PUBLIC_KEY: &str = include_str!("data/TEST_ONLY_public_key");
const HELLO_SIG: &str = include_str!("data/hello.sig");
const UPLOAD_CERT_SIG: &str = include_str!("data/upload_cert.sig");
const SAMPLE_FILE: &[u8] = include_bytes!("data/sample_file.bin");
const SAMPLE_FILE_SIG: &str = include_str!("data/sample_file.sig");
const SAMPLE_FILE_MD5: &str = "57064a471b68e0deaf58405991432dd8";

const HELLO: &str = "hello BOINC";
const CERT_NAME: &str = "fixture_result_0_0";
const CERT_MAX_NBYTES: f64 = 5_000_000.0;

fn private_key() -> PrivateKey {
    PrivateKey::parse(PRIVATE_KEY).expect("private key fixture parses")
}

fn public_key() -> PublicKey {
    PublicKey::parse(PUBLIC_KEY).expect("public key fixture parses")
}

#[test]
fn key_files_round_trip_exactly() {
    assert_eq!(private_key().to_key_string(), PRIVATE_KEY);
    assert_eq!(public_key().to_key_string(), PUBLIC_KEY);
    assert_eq!(public_key().to_string(), PUBLIC_KEY);
    assert_eq!(PUBLIC_KEY.parse::<PublicKey>().unwrap(), public_key());
}

#[test]
fn key_fields() {
    let private = private_key();
    let public = public_key();
    assert_eq!(public.bits, 1024);
    assert_eq!(private.bits, 1024);
    assert_eq!(private.public_key(), public);
    assert_eq!(public.exponent[125..], [0x01, 0x00, 0x01]);
    assert!(public.exponent[..125].iter().all(|&b| b == 0));
}

#[test]
fn public_key_parses_with_trailing_whitespace_stripped() {
    // the scheduler strips whitespace from code_sign_key before sending it
    assert_eq!(
        PublicKey::parse(PUBLIC_KEY.trim_end()).unwrap(),
        public_key()
    );
}

#[test]
fn rsa_conversions_round_trip() {
    let private = private_key();
    let rsa_private = private.to_rsa().unwrap();
    // CRT values recomputed by this crate equal the ones OpenSSL stored.
    assert_eq!(PrivateKey::from_rsa(&rsa_private).unwrap(), private);

    let public = public_key();
    let rsa_public = public.to_rsa().unwrap();
    assert_eq!(PublicKey::from_rsa(&rsa_public).unwrap(), public);
    assert_eq!(rsa_private.to_public_key(), rsa_public);
}

#[test]
fn verifies_crypt_prog_string_signature() {
    let key = public_key();
    assert!(verify_text(HELLO, HELLO_SIG, &key));
    assert!(check_string_signature(HELLO, HELLO_SIG, &key).unwrap());
    assert!(!verify_text("hello BOINC!", HELLO_SIG, &key));
    assert!(!check_string_signature("hello BOINC!", HELLO_SIG, &key).unwrap());
}

#[test]
fn verifies_crypt_prog_file_signature() {
    let key = public_key();
    assert_eq!(md5_hex(SAMPLE_FILE), SAMPLE_FILE_MD5);
    let mut stream = Md5Stream::new();
    for chunk in SAMPLE_FILE.chunks(7) {
        stream.update(chunk);
    }
    assert_eq!(stream.nbytes(), SAMPLE_FILE.len() as u64);
    assert_eq!(stream.finalize_hex(), SAMPLE_FILE_MD5);

    assert!(verify_md5(SAMPLE_FILE_MD5, SAMPLE_FILE_SIG, &key));
    assert!(verify_bytes(SAMPLE_FILE, SAMPLE_FILE_SIG, &key));
    assert!(check_file_signature(SAMPLE_FILE_MD5, SAMPLE_FILE_SIG, &key).unwrap());
    assert!(!verify_md5(&md5_hex("other"), SAMPLE_FILE_SIG, &key));
    assert!(!verify_md5(SAMPLE_FILE_MD5, HELLO_SIG, &key));
    // the comparison is with the exact lowercase digest
    assert!(!verify_md5(
        &SAMPLE_FILE_MD5.to_uppercase(),
        SAMPLE_FILE_SIG,
        &key
    ));
}

#[test]
fn verifies_crypt_prog_upload_certificate() {
    let key = public_key();
    assert!(verify_upload_certificate(
        CERT_NAME,
        CERT_MAX_NBYTES,
        UPLOAD_CERT_SIG,
        &key
    ));
    assert!(!verify_upload_certificate(
        CERT_NAME,
        CERT_MAX_NBYTES + 1.0,
        UPLOAD_CERT_SIG,
        &key
    ));
    assert!(!verify_upload_certificate(
        "fixture_result_0_1",
        CERT_MAX_NBYTES,
        UPLOAD_CERT_SIG,
        &key
    ));
}

#[test]
fn signatures_are_byte_identical_to_crypt_prog() {
    let key = private_key();
    assert_eq!(sign_text(HELLO, &key).unwrap(), HELLO_SIG);
    assert_eq!(sign_md5(SAMPLE_FILE_MD5, &key).unwrap(), SAMPLE_FILE_SIG);
    assert_eq!(sign_bytes(SAMPLE_FILE, &key).unwrap(), SAMPLE_FILE_SIG);
    assert_eq!(
        sign_upload_certificate(CERT_NAME, CERT_MAX_NBYTES, &key).unwrap(),
        UPLOAD_CERT_SIG
    );

    let signer = Signer::new(&key).unwrap();
    assert_eq!(signer.sign_text(HELLO).unwrap(), HELLO_SIG);
    assert_eq!(
        print_hex_data(&signer.encrypt_private(SAMPLE_FILE_MD5.as_bytes()).unwrap()),
        SAMPLE_FILE_SIG
    );
}

#[test]
fn signature_block_shape() {
    let sig = sign_text(HELLO, &private_key()).unwrap();
    let lines: Vec<&str> = sig.split('\n').collect();
    // four lines of 64 hex characters, ".", and the empty piece after the
    // final newline
    assert_eq!(lines.len(), 6);
    assert!(lines[..4].iter().all(|l| l.len() == 64));
    assert_eq!(lines[4], ".");
    assert_eq!(lines[5], "");
    assert_eq!(scan_hex_data(&sig).len(), 128);
}

#[test]
fn sign_md5_refuses_non_md5_input() {
    let key = private_key();
    for bad in [
        "",
        "abc",
        "57064A471B68E0DEAF58405991432DD8",
        "57064a471b68e0deaf58405991432dd",
        "57064a471b68e0deaf58405991432dd8 ",
    ] {
        assert!(matches!(sign_md5(bad, &key), Err(Error::InvalidMd5)));
    }
}

#[test]
fn signature_text_variants() {
    let key = public_key();
    // trailing whitespace stripped, as 7.0+ clients do to XML element contents
    assert!(verify_text(HELLO, HELLO_SIG.trim_end(), &key));
    // without the "." terminator
    assert!(verify_text(HELLO, HELLO_SIG.trim_end_matches(".\n"), &key));
    // uppercase hex digits
    assert!(verify_text(HELLO, &HELLO_SIG.to_uppercase(), &key));
    // all on one line
    assert!(verify_text(HELLO, &HELLO_SIG.replace('\n', ""), &key));
    // leading newline, as in "<xml_signature>\n..."
    assert!(verify_text(HELLO, &format!("\n{HELLO_SIG}"), &key));
    // leading spaces stop the upstream scanner: no data
    assert!(matches!(
        check_string_signature(HELLO, &format!("  {HELLO_SIG}"), &key),
        Err(Error::BadHexFormat)
    ));
    assert!(matches!(
        check_string_signature(HELLO, "", &key),
        Err(Error::BadHexFormat)
    ));
}

#[test]
fn rejects_tampered_signatures() {
    let key = public_key();
    let good = scan_hex_data(HELLO_SIG);
    for i in [0, 1, 64, 127] {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        let block = print_hex_data(&bad);
        assert!(!verify_text(HELLO, &block, &key), "byte {i}");
    }
    // truncated signature
    assert!(!verify_text(HELLO, &print_hex_data(&good[..127]), &key));
    // longer than the modulus
    let mut long = good.clone();
    long.push(0);
    assert!(matches!(
        check_string_signature(HELLO, &print_hex_data(&long), &key),
        Err(Error::Crypto(_))
    ));
    // not less than the modulus
    assert!(matches!(
        check_string_signature(HELLO, &print_hex_data(&[0xff; 128]), &key),
        Err(Error::Crypto(_))
    ));
    // 0 and 1 are fixed points of RSA and are not valid padded messages
    for v in [0u8, 1] {
        let mut s = [0u8; 128];
        s[127] = v;
        assert!(!verify_text(HELLO, &print_hex_data(&s), &key));
    }
}

#[test]
fn wrong_key_does_not_verify() {
    // A different public key: same exponent, modulus changed in one byte
    // (kept odd).
    let mut other = public_key();
    other.modulus[64] ^= 0x10;
    assert!(!verify_text(HELLO, HELLO_SIG, &other));
}

#[test]
fn code_sign_key_rollover_signature() {
    // send_code_sign_key: the new public key text, whitespace-stripped, is
    // signed with the old private key and checked against the old public key.
    let old_private = private_key();
    let old_public = public_key();
    let new_key_text = PUBLIC_KEY.replace("c2d3", "c2d5");
    let stripped = new_key_text.trim_end();
    let signature = sign_text(stripped, &old_private).unwrap();
    assert!(verify_text(stripped, &signature, &old_public));
    assert!(!verify_text(&new_key_text, &signature, &old_public));
}

#[cfg(feature = "keygen")]
#[test]
fn generated_keys_work() {
    let (private, public) = cb_crypto::generate_key_pair().unwrap();
    assert_eq!(public.bits, 1024);
    assert_eq!(private.public_key(), public);
    // larger prime first, as OpenSSL stores them
    assert!(private.prime[0] > private.prime[1]);

    let private_text = private.to_key_string();
    let public_text = public.to_key_string();
    assert_eq!(private_text.len(), PRIVATE_KEY.len());
    assert_eq!(public_text.len(), PUBLIC_KEY.len());
    assert_eq!(PrivateKey::parse(&private_text).unwrap(), private);
    assert_eq!(PublicKey::parse(&public_text).unwrap(), public);
    assert_eq!(
        PrivateKey::from_rsa(&private.to_rsa().unwrap()).unwrap(),
        private
    );

    let sig = sign_text(HELLO, &private).unwrap();
    assert!(verify_text(HELLO, &sig, &public));
    assert!(!verify_text(HELLO, &sig, &public_key()));
    assert!(!verify_text(HELLO, HELLO_SIG, &public));
}
