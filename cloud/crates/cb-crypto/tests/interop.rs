//! Interoperability tests against external programs.
//!
//! * `openssl` (found on `PATH`): the tests are skipped with a note when it
//!   is not installed.
//! * The upstream BOINC `crypt_prog`: used when the environment variable
//!   `CB_CRYPT_PROG` holds the path of a built binary (see
//!   `tests/data/README.txt` for a build recipe); skipped otherwise.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cb_crypto::{
    md5_hex, print_hex_data, scan_hex_data, sign_bytes, sign_text, verify_bytes, verify_text,
    PrivateKey, PublicKey,
};
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey, LineEnding};
use rsa::RsaPrivateKey;

const PRIVATE_KEY: &str = include_str!("data/TEST_ONLY_private_key");
const PUBLIC_KEY: &str = include_str!("data/TEST_ONLY_public_key");
const TEXT: &str = "<name>interop_result_1_0</name><max_nbytes>123456</max_nbytes>";

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cb-crypto-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn have_openssl() -> bool {
    let found = Command::new("openssl")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        eprintln!("skipping: openssl not found on PATH");
    }
    found
}

fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let out = Command::new(program.as_ref())
        .args(args)
        .output()
        .expect("program runs");
    assert!(
        out.status.success(),
        "{:?} {:?} failed: {}{}",
        program.as_ref(),
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

macro_rules! args {
    ($($a:expr),* $(,)?) => { &[$(std::ffi::OsStr::new($a)),*] };
}

/// `openssl pkeyutl -verifyrecover`: the type 1 padded payload of `sig`.
fn openssl_recover(pem: &Path, sig: &Path) -> Vec<u8> {
    run(
        "openssl",
        args![
            "pkeyutl",
            "-verifyrecover",
            "-inkey",
            pem,
            "-pkeyopt",
            "rsa_padding_mode:pkcs1",
            "-in",
            sig
        ],
    )
}

/// `openssl pkeyutl -sign` without a digest: raw type 1 padding over `input`.
fn openssl_sign(pem: &Path, input: &Path, out: &Path) -> Vec<u8> {
    run(
        "openssl",
        args![
            "pkeyutl",
            "-sign",
            "-inkey",
            pem,
            "-pkeyopt",
            "rsa_padding_mode:pkcs1",
            "-in",
            input,
            "-out",
            out
        ],
    );
    fs::read(out).unwrap()
}

#[test]
fn openssl_accepts_our_signature() {
    if !have_openssl() {
        return;
    }
    let dir = Scratch::new("ours");
    let key = PrivateKey::parse(PRIVATE_KEY).unwrap();
    let pem = key.to_rsa().unwrap().to_pkcs1_pem(LineEnding::LF).unwrap();
    let pem_path = dir.write("key.pem", pem.as_bytes());

    let block = sign_text(TEXT, &key).unwrap();
    let sig_path = dir.write("sig.bin", scan_hex_data(&block));

    // OpenSSL recovers exactly the MD5 hex string of the text.
    assert_eq!(
        openssl_recover(&pem_path, &sig_path),
        md5_hex(TEXT).as_bytes()
    );

    // And signing the MD5 hex string with OpenSSL gives the same bytes.
    let md5_path = dir.write("md5.txt", md5_hex(TEXT));
    let openssl_sig = openssl_sign(&pem_path, &md5_path, &dir.path("openssl.sig"));
    assert_eq!(print_hex_data(&openssl_sig), block);
}

#[test]
fn we_accept_openssl_key_and_signature() {
    if !have_openssl() {
        return;
    }
    let dir = Scratch::new("theirs");
    let pem_path = dir.path("key.pem");
    run(
        "openssl",
        args!["genrsa", "-traditional", "-out", &pem_path, "1024"],
    );
    let rsa_key = RsaPrivateKey::read_pkcs1_pem_file(&pem_path).unwrap();
    let private = PrivateKey::from_rsa(&rsa_key).unwrap();
    let public = private.public_key();
    assert_eq!(public.bits, 1024);

    // The BOINC text form survives a round trip.
    assert_eq!(
        PrivateKey::parse(&private.to_key_string()).unwrap(),
        private
    );

    let md5_path = dir.write("md5.txt", md5_hex(TEXT));
    let openssl_sig = openssl_sign(&pem_path, &md5_path, &dir.path("openssl.sig"));
    let block = print_hex_data(&openssl_sig);
    assert!(verify_text(TEXT, &block, &public));
    assert!(!verify_text("something else", &block, &public));
    assert_eq!(sign_text(TEXT, &private).unwrap(), block);
}

fn crypt_prog() -> Option<PathBuf> {
    match std::env::var_os("CB_CRYPT_PROG") {
        Some(p) => Some(PathBuf::from(p)),
        None => {
            eprintln!("skipping: CB_CRYPT_PROG not set");
            None
        }
    }
}

#[test]
fn crypt_prog_accepts_our_signatures() {
    let Some(prog) = crypt_prog() else { return };
    let dir = Scratch::new("crypt-prog-verify");
    let key = PrivateKey::parse(PRIVATE_KEY).unwrap();
    let public_path = dir.write("public", PUBLIC_KEY);

    let string_sig = dir.write("string.sig", sign_text(TEXT, &key).unwrap());
    let out = run(
        &prog,
        args!["-verify_string", TEXT, &string_sig, &public_path],
    );
    assert_eq!(String::from_utf8_lossy(&out).trim(), "signature is valid");

    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let file = dir.write("file.bin", &data);
    let file_sig = dir.write("file.sig", sign_bytes(&data, &key).unwrap());
    let out = run(&prog, args!["-verify", &file, &file_sig, &public_path]);
    assert_eq!(String::from_utf8_lossy(&out).trim(), "signature is valid");

    // A signature of other data is reported invalid, with a failing exit
    // status.
    let out = Command::new(&prog)
        .args(["-verify_string", "other text"])
        .arg(&string_sig)
        .arg(&public_path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "signature is invalid"
    );
}

#[test]
fn we_accept_crypt_prog_key_and_signatures() {
    let Some(prog) = crypt_prog() else { return };
    let dir = Scratch::new("crypt-prog-sign");
    let private_path = dir.path("private");
    let public_path = dir.path("public");
    run(&prog, args!["-genkey", "1024", &private_path, &public_path]);
    let private_text = fs::read_to_string(&private_path).unwrap();
    let public_text = fs::read_to_string(&public_path).unwrap();
    let private = PrivateKey::parse(&private_text).unwrap();
    let public = PublicKey::parse(&public_text).unwrap();
    assert_eq!(private.to_key_string(), private_text);
    assert_eq!(public.to_key_string(), public_text);
    assert_eq!(private.public_key(), public);
    assert_eq!(
        PrivateKey::from_rsa(&private.to_rsa().unwrap()).unwrap(),
        private
    );

    let block = String::from_utf8(run(&prog, args!["-sign_string", TEXT, &private_path])).unwrap();
    assert!(verify_text(TEXT, &block, &public));
    assert_eq!(sign_text(TEXT, &private).unwrap(), block);

    let data = b"file contents\n".repeat(1000);
    let file = dir.write("file.bin", &data);
    let block = String::from_utf8(run(&prog, args!["-sign", &file, &private_path])).unwrap();
    assert!(verify_bytes(&data, &block, &public));
    assert_eq!(sign_bytes(&data, &private).unwrap(), block);
}

#[cfg(feature = "keygen")]
#[test]
fn crypt_prog_accepts_our_generated_key() {
    let Some(prog) = crypt_prog() else { return };
    let dir = Scratch::new("crypt-prog-keygen");
    let (private, public) = cb_crypto::generate_key_pair().unwrap();
    let private_path = dir.write("private", private.to_key_string());
    let public_path = dir.write("public", public.to_key_string());

    // crypt_prog signs with our private key file; we verify.
    let block = String::from_utf8(run(&prog, args!["-sign_string", TEXT, &private_path])).unwrap();
    assert!(verify_text(TEXT, &block, &public));
    assert_eq!(sign_text(TEXT, &private).unwrap(), block);

    // crypt_prog verifies our signature with our public key file.
    let sig = dir.write("string.sig", sign_text(TEXT, &private).unwrap());
    let out = run(&prog, args!["-verify_string", TEXT, &sig, &public_path]);
    assert_eq!(String::from_utf8_lossy(&out).trim(), "signature is valid");
}
