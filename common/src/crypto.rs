use argon2::{
    password_hash::{PasswordHasher, SaltString},
    Argon2, ParamsBuilder, Version,
};
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    ChaCha20Poly1305, Nonce,
};
use rand::RngCore;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;

#[derive(Error, Debug)]
pub enum CryptoError {
    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("Key derivation failed: {0}")]
    KeyDerivationFailed(String),

    #[error("Invalid nonce")]
    InvalidNonce,
}

/// Crypto handler for encrypting/decrypting messages
pub struct Crypto {
    cipher: ChaCha20Poly1305,
    nonce_counter: AtomicU64,
}

impl Crypto {
    /// Create a new Crypto instance from password
    pub fn new(password: &str) -> Result<Self, CryptoError> {
        let key = Self::derive_key(password)?;
        let cipher = ChaCha20Poly1305::new(&key.into());

        Ok(Self {
            cipher,
            nonce_counter: AtomicU64::new(0),
        })
    }

    /// Derive encryption key from password using Argon2id
    fn derive_key(password: &str) -> Result<[u8; 32], CryptoError> {
        // Use a fixed salt for deterministic key derivation
        // In production, consider using a random salt exchanged during handshake
        let salt = SaltString::encode_b64(b"tunnix-salt-v1-fixed-32bytes!!!!").unwrap();

        let params = ParamsBuilder::new()
            .m_cost(19456) // 19 MB memory
            .t_cost(2)     // 2 iterations
            .p_cost(1)     // 1 thread
            .build()
            .map_err(|e| CryptoError::KeyDerivationFailed(e.to_string()))?;

        let argon2 = Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params);

        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| CryptoError::KeyDerivationFailed(e.to_string()))?;

        let hash_bytes = password_hash
            .hash
            .ok_or_else(|| CryptoError::KeyDerivationFailed("No hash output".to_string()))?;

        let mut key = [0u8; 32];
        key.copy_from_slice(&hash_bytes.as_bytes()[..32]);

        Ok(key)
    }

    /// Generate a unique nonce
    fn generate_nonce(&self) -> [u8; NONCE_SIZE] {
        let counter = self.nonce_counter.fetch_add(1, Ordering::SeqCst);
        let mut nonce = [0u8; NONCE_SIZE];

        // Use counter + random bytes for uniqueness
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        OsRng.fill_bytes(&mut nonce[8..]);

        nonce
    }

    /// Encrypt plaintext
    ///
    /// Returns: [nonce (12 bytes)][ciphertext][tag (16 bytes)]
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes = self.generate_nonce();
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| CryptoError::EncryptionFailed)?;

        // Prepend nonce to ciphertext
        let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);

        Ok(result)
    }

    /// Decrypt ciphertext
    ///
    /// Expects: [nonce (12 bytes)][ciphertext][tag (16 bytes)]
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if data.len() < NONCE_SIZE + TAG_SIZE {
            return Err(CryptoError::InvalidNonce);
        }

        let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
        let nonce = Nonce::from_slice(nonce_bytes);

        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| CryptoError::DecryptionFailed)?;

        Ok(plaintext)
    }
}

impl Drop for Crypto {
    fn drop(&mut self) {
        // Zeroize is handled by ChaCha20Poly1305's Drop impl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_derivation() {
        let key1 = Crypto::derive_key("test-password").unwrap();
        let key2 = Crypto::derive_key("test-password").unwrap();

        // Same password should produce same key (deterministic)
        assert_eq!(key1, key2);

        let key3 = Crypto::derive_key("different-password").unwrap();
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_encryption_decryption() {
        let crypto = Crypto::new("test-password-123").unwrap();
        let plaintext = b"Hello, World! This is a test message.";

        let encrypted = crypto.encrypt(plaintext).unwrap();
        let decrypted = crypto.decrypt(&encrypted).unwrap();

        assert_eq!(plaintext.as_slice(), decrypted.as_slice());
    }

    #[test]
    fn test_different_keys_fail() {
        let crypto1 = Crypto::new("password1").unwrap();
        let crypto2 = Crypto::new("password2").unwrap();

        let plaintext = b"Secret message";
        let encrypted = crypto1.encrypt(plaintext).unwrap();

        // Should fail to decrypt with different password
        assert!(crypto2.decrypt(&encrypted).is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let crypto = Crypto::new("test-password").unwrap();
        let plaintext = b"Important data";

        let mut encrypted = crypto.encrypt(plaintext).unwrap();

        // Tamper with the ciphertext
        if let Some(byte) = encrypted.last_mut() {
            *byte ^= 0xFF;
        }

        // Should fail due to authentication tag mismatch
        assert!(crypto.decrypt(&encrypted).is_err());
    }

    #[test]
    fn test_unique_nonces() {
        let crypto = Crypto::new("test").unwrap();
        let plaintext = b"Same message";

        let encrypted1 = crypto.encrypt(plaintext).unwrap();
        let encrypted2 = crypto.encrypt(plaintext).unwrap();

        // Same plaintext should produce different ciphertext due to unique nonces
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to same plaintext
        assert_eq!(crypto.decrypt(&encrypted1).unwrap(), plaintext);
        assert_eq!(crypto.decrypt(&encrypted2).unwrap(), plaintext);
    }
}
