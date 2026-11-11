use core::fmt;

/// Errors from key parsing, signing and signature checking.
#[derive(Debug)]
pub enum Error {
    /// A key file is not in the format of upstream `print_key_hex`.
    /// Corresponds to upstream `ERR_BAD_HEX_FORMAT` / a failed `scan_key_hex`.
    BadKeyFormat(&'static str),
    /// A signature block holds no hex data (upstream `ERR_BAD_HEX_FORMAT`).
    BadHexFormat,
    /// The key does not fit the fixed-size upstream key structures
    /// (`MAX_RSA_MODULUS_BITS` is 1024).
    KeyTooLarge {
        /// Bit length of the offending modulus.
        bits: usize,
    },
    /// The value passed as an MD5 is not 32 lowercase hex characters.
    InvalidMd5,
    /// The RSA operation failed (upstream `ERR_CRYPTO`).
    Crypto(&'static str),
    /// An error reported by the `rsa` crate.
    Rsa(rsa::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadKeyFormat(why) => write!(f, "bad key format: {why}"),
            Error::BadHexFormat => f.write_str("bad hex format"),
            Error::KeyTooLarge { bits } => write!(
                f,
                "{bits}-bit RSA modulus exceeds the BOINC limit of {} bits",
                crate::key::MAX_RSA_MODULUS_BITS
            ),
            Error::InvalidMd5 => f.write_str("MD5 must be 32 lowercase hex characters"),
            Error::Crypto(why) => write!(f, "RSA operation failed: {why}"),
            Error::Rsa(e) => write!(f, "RSA error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rsa::Error> for Error {
    fn from(e: rsa::Error) -> Self {
        Error::Rsa(e)
    }
}
