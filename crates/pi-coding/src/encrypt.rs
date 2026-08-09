//! Passphrase-based AES-256-GCM encryption for session shares.
//!
//! # Scheme (interoperability reference)
//!
//! - **Key derivation**: PBKDF2-HMAC-SHA256 with **210,000 iterations**
//!   ([`PBKDF2_ITERATIONS`], the OWASP 2023 recommendation) and a fresh
//!   16-byte random salt per encryption ([`SALT_LEN`]). The raw passphrase is
//!   never stored, hashed beyond the key derivation, or logged.
//! - **Salt**: 16 fresh random bytes per encryption, prefixed to the payload
//!   so decryption can re-derive the key.
//! - **Nonce**: 12 fresh random bytes per encryption (the AES-GCM standard
//!   96-bit nonce).
//! - **File layout**: `salt (16 bytes) || nonce (12 bytes) || AES-256-GCM
//!   ciphertext` where the ciphertext includes the 16-byte authentication
//!   tag. Decryption splits the salt, re-derives the key, splits the nonce,
//!   authenticates the tag, and returns the plaintext.
//!
//! Implemented on [`ring::aead::AES_256_GCM`] and [`ring::pbkdf2`] (ring
//! 0.17, already in the dependency tree via rustls — no new crate download).
//! Authenticated encryption means a wrong passphrase fails on tag
//! verification, and any tampering with the ciphertext is rejected. The
//! salted PBKDF2 derivation means the same passphrase yields a different key
//! per encryption, so precomputed (rainbow) tables cannot attack the
//! passphrase.
//!
//! There is no format migration: encrypted session shares are a new feature,
//! so every share uses the `salt || nonce || ciphertext+tag` layout above.

use std::num::NonZeroU32;

use anyhow::{Result, anyhow, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};

/// Length of the random salt prefix in bytes.
pub const SALT_LEN: usize = 16;

/// Length of the random nonce prefix in bytes (AES-GCM standard 96-bit nonce).
pub const NONCE_LEN: usize = 12;

/// PBKDF2-HMAC-SHA256 iteration count (OWASP 2023 recommendation: 210,000).
/// Deliberately a named public constant so the scheme is auditable and
/// decryptable by other implementations.
pub const PBKDF2_ITERATIONS: u32 = 210_000;

/// Length of the derived AES-256 key in bytes.
const KEY_LEN: usize = 32;

/// Derive the 32-byte AES-256 key from a passphrase via PBKDF2-HMAC-SHA256
/// with `salt` and [`PBKDF2_ITERATIONS`] iterations.
///
/// The salt is random per encryption (see [`encrypt`]), so the same
/// passphrase never yields the same key twice.
#[must_use]
pub fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERATIONS).expect("pbkdf2 iterations are non-zero"),
        salt,
        passphrase.as_bytes(),
        &mut key,
    );
    key
}

/// Encrypt `plaintext` under `passphrase`.
///
/// Returns `salt (16 bytes) || nonce (12 bytes) || ciphertext+tag` — the
/// on-disk layout of an encrypted session share. A fresh random salt and
/// nonce are generated per call, so encrypting the same plaintext twice
/// yields different salts, keys, and ciphertexts.
pub fn encrypt(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("generating encryption salt"))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &derive_key(passphrase, &salt))
            .map_err(|_| anyhow!("AES-256-GCM rejected the derived key"))?,
    );
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| anyhow!("generating encryption nonce"))?;
    let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes)
        .map_err(|_| anyhow!("generated an invalid AES-GCM nonce"))?;
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow!("AES-256-GCM encryption failed"))?;
    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + in_out.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Ok(out)
}

/// Decrypt `data` (`salt || nonce || ciphertext+tag`) under `passphrase`.
///
/// Fails on a wrong passphrase (tag verification) or on any payload shorter
/// than the salt+nonce prefix or corrupted by tampering.
pub fn decrypt(passphrase: &str, data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < SALT_LEN + NONCE_LEN {
        bail!(
            "encrypted payload is too short; missing the {SALT_LEN}-byte salt and \
             {NONCE_LEN}-byte nonce prefix"
        );
    }
    let (salt_bytes, rest) = data.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &derive_key(passphrase, salt_bytes))
            .map_err(|_| anyhow!("AES-256-GCM rejected the derived key"))?,
    );
    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes)
        .map_err(|_| anyhow!("invalid nonce prefix"))?;
    let mut in_out = ciphertext.to_vec();
    // open_in_place returns the authenticated plaintext slice; the tag stays
    // appended in the backing Vec, so copy only the returned portion.
    Ok(key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow!("decryption failed: wrong passphrase or corrupted data"))?
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_key_is_salted_pbkdf2_and_deterministic() {
        let salt = [7u8; SALT_LEN];
        let key = derive_key("hunter2", &salt);
        let expected = derive_key("hunter2", &salt);
        assert_eq!(key, expected, "same salt must derive the same key");
        assert_eq!(key.len(), KEY_LEN);
        // A different salt must derive a different key from the same
        // passphrase.
        let other_salt = [9u8; SALT_LEN];
        assert_ne!(key, derive_key("hunter2", &other_salt));
    }

    #[test]
    fn iterations_constant_is_the_owasp_2023_recommendation() {
        assert_eq!(PBKDF2_ITERATIONS, 210_000);
    }

    #[test]
    fn round_trip_restores_plaintext() {
        let plaintext = b"{\"type\":\"session\",\"version\":3}\n";
        let encrypted = encrypt("correct horse", plaintext).expect("encrypt");
        let decrypted = decrypt("correct horse", &encrypted).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_passphrase_fails_on_tag_verification() {
        let plaintext = b"secret session content";
        let encrypted = encrypt("right", plaintext).expect("encrypt");
        let error = decrypt("wrong", &encrypted).expect_err("wrong passphrase must fail");
        assert!(error.to_string().contains("decryption failed"));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let plaintext = b"secret session content";
        let mut encrypted = encrypt("right", plaintext).expect("encrypt");
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0x01;
        assert!(decrypt("right", &encrypted).is_err());
    }

    #[test]
    fn short_payload_is_rejected() {
        let error = decrypt("right", &[0u8; 8]).expect_err("short payload must fail");
        assert!(error.to_string().contains("salt and"));
        assert!(decrypt("right", &[]).is_err());
    }

    #[test]
    fn salt_and_nonce_prefixes_are_unique_per_encryption() {
        let plaintext = b"same plaintext";
        let first = encrypt("pass", plaintext).expect("encrypt");
        let second = encrypt("pass", plaintext).expect("encrypt");
        assert_eq!(first.len(), second.len());
        assert_ne!(
            first[..SALT_LEN],
            second[..SALT_LEN],
            "each encryption must use a fresh random salt"
        );
        assert_ne!(
            first[SALT_LEN..SALT_LEN + NONCE_LEN],
            second[SALT_LEN..SALT_LEN + NONCE_LEN],
            "each encryption must use a fresh nonce"
        );
    }

    #[test]
    fn payload_starts_with_salt_then_nonce_then_ciphertext() {
        let plaintext = b"layout probe";
        let encrypted = encrypt("pass", plaintext).expect("encrypt");
        assert_eq!(encrypted.len(), SALT_LEN + NONCE_LEN + plaintext.len() + 16);
        // The plaintext must never appear verbatim in the output.
        assert!(
            !encrypted
                .windows(plaintext.len())
                .any(|window| window == plaintext),
            "ciphertext must not contain the plaintext"
        );
        // The salt and nonce are random bytes, but the layout is fixed:
        // salt(16) || nonce(12) || ciphertext+tag. Decrypting with the right
        // passphrase proves the layout round-trips; a wrong key fails on the
        // tag regardless of where the boundary sits.
        let decrypted = decrypt("pass", &encrypted).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn empty_passphrase_is_allowed_but_distinct() {
        let plaintext = b"data";
        let encrypted = encrypt("", plaintext).expect("encrypt");
        assert_eq!(decrypt("", &encrypted).expect("decrypt"), plaintext);
        // An empty passphrase is distinct from a non-empty one.
        let error = decrypt("x", &encrypted).expect_err("different passphrase must fail");
        assert!(error.to_string().contains("decryption failed"));
    }

    #[test]
    fn different_passphrase_fails_on_tag_verification() {
        let plaintext = b"secret session content";
        let encrypted = encrypt("right", plaintext).expect("encrypt");
        let error = decrypt("x", &encrypted).expect_err("different passphrase must fail");
        assert!(error.to_string().contains("decryption failed"));
    }
}
