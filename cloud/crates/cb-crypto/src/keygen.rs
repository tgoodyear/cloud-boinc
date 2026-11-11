//! Key pair generation (cargo feature `keygen`).
//!
//! Upstream: `genkey` in `lib/crypt_prog.cpp`, which only accepts 1024 bits.

use rsa::rand_core::{CryptoRngCore, OsRng};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::RsaPrivateKey;

use crate::error::Error;
use crate::key::{PrivateKey, PublicKey, MAX_RSA_MODULUS_BITS};

/// Generate a 1024-bit key pair with public exponent 65537, using the
/// operating system's random number generator.
pub fn generate_key_pair() -> Result<(PrivateKey, PublicKey), Error> {
    generate_key_pair_with_rng(&mut OsRng)
}

/// Generate a 1024-bit key pair with public exponent 65537 from the given
/// cryptographically secure random number generator.
///
/// The primes are stored larger first, as OpenSSL does.
pub fn generate_key_pair_with_rng<R: CryptoRngCore + ?Sized>(
    rng: &mut R,
) -> Result<(PrivateKey, PublicKey), Error> {
    let key = RsaPrivateKey::new(rng, MAX_RSA_MODULUS_BITS)?;
    let mut primes = key.primes().to_vec();
    primes.sort_by(|a, b| b.cmp(a));
    let key =
        RsaPrivateKey::from_components(key.n().clone(), key.e().clone(), key.d().clone(), primes)?;
    let private = PrivateKey::from_rsa(&key)?;
    let public = private.public_key();
    Ok((private, public))
}
