//! BOINC key files.
//!
//! Upstream keeps keys in two fixed-size C structures, `R_RSA_PUBLIC_KEY` and
//! `R_RSA_PRIVATE_KEY` (`lib/crypt.h`), and stores them as text: the bit
//! count in decimal on the first line, then every byte of the structure after
//! the bit count as a hex block (`print_key_hex` / `scan_key_hex` in
//! `lib/crypt.cpp`). Each number is big-endian and right-aligned in its
//! field, padded on the left with zeros.
//!
//! # Key sizes
//!
//! `MAX_RSA_MODULUS_BITS` is 1024 upstream, and `crypt_prog -genkey` refuses
//! any other size. Because the structures are fixed-size, a key file always
//! has the same length whatever the modulus size:
//!
//! * a smaller key (for example 512 bits) fits, with more zero padding and a
//!   smaller bit count on the first line, and works with a stock client;
//! * a larger key cannot be represented, and conversion from an [`rsa`] key
//!   of more than 1024 bits fails with [`Error::KeyTooLarge`].

use core::fmt;

use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use zeroize::Zeroize;

use crate::error::Error;
use crate::hexfmt::{print_hex_data, scan_hex_data};

/// Upstream `MAX_RSA_MODULUS_BITS`.
pub const MAX_RSA_MODULUS_BITS: usize = 1024;
/// Upstream `MAX_RSA_MODULUS_LEN`: bytes in a modulus-sized field.
pub const MAX_RSA_MODULUS_LEN: usize = MAX_RSA_MODULUS_BITS.div_ceil(8);
/// Upstream `MAX_RSA_PRIME_BITS`.
pub const MAX_RSA_PRIME_BITS: usize = MAX_RSA_MODULUS_BITS.div_ceil(2);
/// Upstream `MAX_RSA_PRIME_LEN`: bytes in a prime-sized field.
pub const MAX_RSA_PRIME_LEN: usize = MAX_RSA_PRIME_BITS.div_ceil(8);

type ModulusField = [u8; MAX_RSA_MODULUS_LEN];
type PrimeField = [u8; MAX_RSA_PRIME_LEN];

/// Bytes following the bit count in `R_RSA_PUBLIC_KEY`.
const PUBLIC_DATA_LEN: usize = 2 * MAX_RSA_MODULUS_LEN;
/// Bytes following the bit count in `R_RSA_PRIVATE_KEY`.
const PRIVATE_DATA_LEN: usize = 3 * MAX_RSA_MODULUS_LEN + 5 * MAX_RSA_PRIME_LEN;

/// A BOINC public key: upstream `R_RSA_PUBLIC_KEY`.
///
/// This is what a project publishes as its `code_sign_key` and what the
/// upload handler reads from the `upload_public` key file.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// Modulus size in bits, as written on the first line of the key file.
    pub bits: u16,
    /// Modulus `n`, big-endian, left-padded with zeros.
    pub modulus: ModulusField,
    /// Public exponent `e`, big-endian, left-padded with zeros.
    pub exponent: ModulusField,
}

/// A BOINC private key: upstream `R_RSA_PRIVATE_KEY`.
///
/// The key material is wiped when the value is dropped, and `Debug` output
/// does not include it.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateKey {
    /// Modulus size in bits, as written on the first line of the key file.
    pub bits: u16,
    /// Modulus `n`.
    pub modulus: ModulusField,
    /// Public exponent `e`.
    pub public_exponent: ModulusField,
    /// Private exponent `d`.
    pub exponent: ModulusField,
    /// Primes `p` and `q`.
    pub prime: [PrimeField; 2],
    /// CRT exponents `d mod (p-1)` and `d mod (q-1)`.
    pub prime_exponent: [PrimeField; 2],
    /// CRT coefficient `q^-1 mod p`.
    pub coefficient: PrimeField,
}

/// Parse the common layout of a key file, as upstream `sscan_key_hex` does:
/// decimal digits up to the first newline give the bit count, and the rest is
/// a hex block. Returns the bit count and the decoded bytes.
fn scan_key_hex(text: &str, expected_len: usize) -> Result<(u16, Vec<u8>), Error> {
    let (first_line, rest) = text
        .split_once('\n')
        .ok_or(Error::BadKeyFormat("no newline after the bit count"))?;
    if first_line.is_empty() || !first_line.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Error::BadKeyFormat("first line is not a decimal bit count"));
    }
    // Upstream accumulates into an unsigned short and would wrap; a value
    // that does not fit is rejected here instead.
    let bits: u16 = first_line
        .parse()
        .map_err(|_| Error::BadKeyFormat("bit count out of range"))?;
    if bits == 0 {
        return Err(Error::BadKeyFormat("bit count is zero"));
    }
    let data = scan_hex_data(rest);
    if data.len() != expected_len {
        let mut data = data;
        data.zeroize();
        return Err(Error::BadKeyFormat("wrong amount of key data"));
    }
    Ok((bits, data))
}

/// Copy a big-endian number into a fixed field, right-aligned, as upstream
/// `bn2bin` does.
fn to_field<const N: usize>(value: &BigUint) -> Result<[u8; N], Error> {
    let mut bytes = value.to_bytes_be();
    if bytes.len() > N {
        bytes.zeroize();
        return Err(Error::KeyTooLarge { bits: value.bits() });
    }
    let mut field = [0u8; N];
    field[N - bytes.len()..].copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(field)
}

fn bit_count(n: &BigUint) -> Result<u16, Error> {
    let bits = n.bits();
    if bits > MAX_RSA_MODULUS_BITS {
        return Err(Error::KeyTooLarge { bits });
    }
    Ok(bits as u16)
}

impl PublicKey {
    /// Parse a public key file (upstream `scan_public_key_hex`).
    ///
    /// The text must start with the bit count; the hex block may end with the
    /// usual `.` line or at the end of the text (the scheduler strips
    /// trailing whitespace from `code_sign_key`, which is still parseable).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (bits, data) = scan_key_hex(text, PUBLIC_DATA_LEN)?;
        let (modulus, exponent) = data.split_at(MAX_RSA_MODULUS_LEN);
        Ok(Self {
            bits,
            modulus: modulus.try_into().expect("length checked"),
            exponent: exponent.try_into().expect("length checked"),
        })
    }

    /// Render the key file text (upstream `print_public_key_hex`).
    pub fn to_key_string(&self) -> String {
        let mut data = Vec::with_capacity(PUBLIC_DATA_LEN);
        data.extend_from_slice(&self.modulus);
        data.extend_from_slice(&self.exponent);
        format!("{}\n{}", self.bits, print_hex_data(&data))
    }

    /// The modulus as a number.
    pub(crate) fn n(&self) -> BigUint {
        BigUint::from_bytes_be(&self.modulus)
    }

    /// The public exponent as a number.
    pub(crate) fn e(&self) -> BigUint {
        BigUint::from_bytes_be(&self.exponent)
    }

    /// Convert to an [`RsaPublicKey`] (upstream `public_to_openssl`).
    pub fn to_rsa(&self) -> Result<RsaPublicKey, Error> {
        Ok(RsaPublicKey::new(self.n(), self.e())?)
    }

    /// Convert from an [`RsaPublicKey`] (upstream `openssl_to_public`).
    /// Fails if the modulus is larger than [`MAX_RSA_MODULUS_BITS`].
    pub fn from_rsa(key: &RsaPublicKey) -> Result<Self, Error> {
        Ok(Self {
            bits: bit_count(key.n())?,
            modulus: to_field(key.n())?,
            exponent: to_field(key.e())?,
        })
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_key_string())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicKey")
            .field("bits", &self.bits)
            .field("modulus", &hex::encode(self.modulus))
            .field("exponent", &self.e())
            .finish()
    }
}

impl core::str::FromStr for PublicKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::parse(s)
    }
}

impl PrivateKey {
    /// Parse a private key file (upstream `scan_private_key_hex`, as used by
    /// `read_key_file`).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (bits, mut data) = scan_key_hex(text, PRIVATE_DATA_LEN)?;
        let m = MAX_RSA_MODULUS_LEN;
        let p = MAX_RSA_PRIME_LEN;
        let modulus_field = |i: usize| -> ModulusField {
            data[i * m..(i + 1) * m].try_into().expect("length checked")
        };
        let prime_field = |i: usize| -> PrimeField {
            data[3 * m + i * p..3 * m + (i + 1) * p]
                .try_into()
                .expect("length checked")
        };
        let key = Self {
            bits,
            modulus: modulus_field(0),
            public_exponent: modulus_field(1),
            exponent: modulus_field(2),
            prime: [prime_field(0), prime_field(1)],
            prime_exponent: [prime_field(2), prime_field(3)],
            coefficient: prime_field(4),
        };
        data.zeroize();
        Ok(key)
    }

    /// Render the key file text (upstream `print_private_key_hex`).
    ///
    /// The result holds the private key; treat it as a secret.
    pub fn to_key_string(&self) -> String {
        let mut data = Vec::with_capacity(PRIVATE_DATA_LEN);
        data.extend_from_slice(&self.modulus);
        data.extend_from_slice(&self.public_exponent);
        data.extend_from_slice(&self.exponent);
        data.extend_from_slice(&self.prime[0]);
        data.extend_from_slice(&self.prime[1]);
        data.extend_from_slice(&self.prime_exponent[0]);
        data.extend_from_slice(&self.prime_exponent[1]);
        data.extend_from_slice(&self.coefficient);
        let text = format!("{}\n{}", self.bits, print_hex_data(&data));
        data.zeroize();
        text
    }

    /// The matching public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey {
            bits: self.bits,
            modulus: self.modulus,
            exponent: self.public_exponent,
        }
    }

    /// Convert to an [`RsaPrivateKey`] (upstream `private_to_openssl`).
    ///
    /// The CRT values are recomputed from the primes rather than taken from
    /// the file. If the file holds no primes (all zero), they are recovered
    /// from `n`, `e` and `d`.
    pub fn to_rsa(&self) -> Result<RsaPrivateKey, Error> {
        let n = BigUint::from_bytes_be(&self.modulus);
        let e = BigUint::from_bytes_be(&self.public_exponent);
        let d = BigUint::from_bytes_be(&self.exponent);
        let primes: Vec<BigUint> = self
            .prime
            .iter()
            .map(|p| BigUint::from_bytes_be(p))
            .filter(|p| p.bits() != 0)
            .collect();
        let primes = if primes.len() == 2 {
            primes
        } else {
            Vec::new()
        };
        let key = RsaPrivateKey::from_components(n, e, d, primes)?;
        key.validate()?;
        Ok(key)
    }

    /// Convert from an [`RsaPrivateKey`] (upstream `openssl_to_private`).
    ///
    /// Fails if the modulus is larger than [`MAX_RSA_MODULUS_BITS`] or the
    /// key does not have exactly two primes.
    pub fn from_rsa(key: &RsaPrivateKey) -> Result<Self, Error> {
        let [p, q] = key.primes() else {
            return Err(Error::Crypto("key must have exactly two primes"));
        };
        let one = BigUint::from(1u8);
        let mut dp = key.d() % (p - &one);
        let mut dq = key.d() % (q - &one);
        let mut qinv = key
            .crt_coefficient()
            .ok_or(Error::Crypto("no CRT coefficient"))?;
        let result = (|| {
            Ok(Self {
                bits: bit_count(key.n())?,
                modulus: to_field(key.n())?,
                public_exponent: to_field(key.e())?,
                exponent: to_field(key.d())?,
                prime: [to_field(p)?, to_field(q)?],
                prime_exponent: [to_field(&dp)?, to_field(&dq)?],
                coefficient: to_field(&qinv)?,
            })
        })();
        dp.zeroize();
        dq.zeroize();
        qinv.zeroize();
        result
    }
}

impl Drop for PrivateKey {
    fn drop(&mut self) {
        self.exponent.zeroize();
        self.prime.zeroize();
        self.prime_exponent.zeroize();
        self.coefficient.zeroize();
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateKey")
            .field("bits", &self.bits)
            .field("modulus", &hex::encode(self.modulus))
            .finish_non_exhaustive()
    }
}

impl core::str::FromStr for PrivateKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_sizes_match_upstream() {
        // sizeof(R_RSA_PUBLIC_KEY) == 258 and sizeof(R_RSA_PRIVATE_KEY) == 706,
        // of which 2 bytes are the bit count.
        assert_eq!(PUBLIC_DATA_LEN, 256);
        assert_eq!(PRIVATE_DATA_LEN, 704);
        assert_eq!(MAX_RSA_MODULUS_LEN, 128);
        assert_eq!(MAX_RSA_PRIME_LEN, 64);
    }

    #[test]
    fn rejects_malformed_keys() {
        let body = print_hex_data(&[1u8; PUBLIC_DATA_LEN]);
        assert!(PublicKey::parse(&format!("1024\n{body}")).is_ok());
        // stripped trailing whitespace and "." still parse
        assert!(PublicKey::parse(format!("1024\n{body}").trim_end_matches(".\n")).is_ok());
        for bad in [
            String::new(),
            "1024".to_string(),
            format!("\n{body}"),
            format!("0\n{body}"),
            format!("10x4\n{body}"),
            format!(" 1024\n{body}"),
            format!("1024\r\n{body}"),
            format!("99999999\n{body}"),
            format!("1024\n{}", print_hex_data(&[1u8; PUBLIC_DATA_LEN - 1])),
            format!("1024\n{}", print_hex_data(&[1u8; PRIVATE_DATA_LEN])),
        ] {
            assert!(PublicKey::parse(&bad).is_err(), "accepted {bad:?}");
        }
        assert!(PrivateKey::parse(&format!("1024\n{body}")).is_err());
    }

    #[test]
    fn debug_hides_private_material() {
        let key = PrivateKey {
            bits: 1024,
            modulus: [0xaa; 128],
            public_exponent: [0; 128],
            exponent: [0xbb; 128],
            prime: [[0xcc; 64]; 2],
            prime_exponent: [[0xdd; 64]; 2],
            coefficient: [0xee; 64],
        };
        let dbg = format!("{key:?}");
        assert!(dbg.contains("aaaa"));
        for secret in ["bbbb", "cccc", "dddd", "eeee"] {
            assert!(!dbg.contains(secret));
        }
    }
}
