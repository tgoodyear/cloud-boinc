//! MD5 helpers matching upstream `md5_block` and `md5_file` in
//! `lib/md5_file.cpp`: the digest is always handled as 32 lowercase hex
//! characters.

use md5::{Digest, Md5};

/// Length of an MD5 digest in hex characters (upstream `MD5_LEN` minus the
/// terminating NUL).
pub const MD5_HEX_LEN: usize = 32;

/// MD5 of `data` as 32 lowercase hex characters (upstream `md5_block` /
/// `md5_string`).
pub fn md5_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(Md5::digest(data.as_ref()))
}

/// True if `s` is 32 lowercase hex characters, the only form the stock
/// client and server ever produce.
pub fn is_md5_hex(s: &str) -> bool {
    s.len() == MD5_HEX_LEN && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Incremental MD5, for data that arrives in pieces (upstream `md5_file`
/// reads the file in 4096-byte blocks).
#[derive(Clone, Default)]
pub struct Md5Stream {
    inner: Md5,
    nbytes: u64,
}

impl Md5Stream {
    /// Start a new digest.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed more data.
    pub fn update(&mut self, data: impl AsRef<[u8]>) {
        let data = data.as_ref();
        self.nbytes += data.len() as u64;
        self.inner.update(data);
    }

    /// Number of bytes fed so far (upstream `md5_file` reports this as
    /// `nbytes`).
    pub fn nbytes(&self) -> u64 {
        self.nbytes
    }

    /// Finish and return the digest as 32 lowercase hex characters.
    pub fn finalize_hex(self) -> String {
        hex::encode(self.inner.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn stream_matches_one_shot() {
        let mut s = Md5Stream::new();
        s.update("a");
        s.update(b"bc");
        assert_eq!(s.nbytes(), 3);
        assert_eq!(s.finalize_hex(), md5_hex("abc"));
    }

    #[test]
    fn md5_hex_check() {
        assert!(is_md5_hex("900150983cd24fb0d6963f7d28e17f72"));
        assert!(!is_md5_hex("900150983CD24FB0D6963F7D28E17F72"));
        assert!(!is_md5_hex("900150983cd24fb0d6963f7d28e17f7"));
        assert!(!is_md5_hex("900150983cd24fb0d6963f7d28e17f7g"));
    }
}
