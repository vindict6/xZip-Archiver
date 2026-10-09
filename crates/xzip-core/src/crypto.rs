//! Password protection.
//!
//! The password goes through Argon2id (64 MiB, 3 passes) to make a key-encryption
//! key, which wraps a random 256-bit archive key. The manifest and every stream are
//! sealed with AES-256-GCM under that archive key, each with its own random nonce.
//! Nothing here is secret except the password: the source, the layout and the
//! parameters are all public, and the security rests on the password's strength and
//! on the cost of Argon2id. A wrong password fails the key unwrap; it is never
//! "nearly right".
//!
//! Crypto block (128 B, right after the header when the ENCRYPTED flag is set):
//!   kdf id u8 (1 = Argon2id), memory KiB u32, passes u32, lanes u32, salt 16 B,
//!   wrapped key 48 B (32 B key + 16 B tag), reserved, CRC32.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::Zeroize;

use crate::error::{Error, Result};

pub const FLAG_ENCRYPTED: u32 = 1;
pub const CRYPTO_SIZE: usize = 128;
pub const OVERHEAD: usize = NONCE_LEN + TAG_LEN;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KDF_ARGON2ID: u8 = 1;
const M_COST_KIB: u32 = 64 * 1024;
const T_COST: u32 = 3;
const P_COST: u32 = 1;
// a hostile archive must not be able to ask for terabytes of memory or hours of CPU
const MAX_M_COST_KIB: u32 = 1 << 20;
const MAX_T_COST: u32 = 32;
const MAX_P_COST: u32 = 16;

/// The archive key. Wiped from memory when dropped.
pub struct Key([u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Clone for Key {
    fn clone(&self) -> Self {
        Key(self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CryptoBlock {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: [u8; 16],
    pub wrapped: [u8; 48],
}

fn random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| Error::Archive(format!("random generator unavailable: {e}")))
}

fn derive(password: &[u8], block: &CryptoBlock) -> Result<Key> {
    let params = argon2::Params::new(block.m_cost_kib, block.t_cost, block.p_cost, Some(32))
        .map_err(|e| Error::Archive(format!("bad key derivation parameters: {e}")))?;
    let kdf = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    kdf.hash_password_into(password, &block.salt, &mut out)
        .map_err(|e| Error::Archive(format!("key derivation failed: {e}")))?;
    Ok(Key(out))
}

/// Make a fresh crypto block and archive key for a new archive.
pub fn new_block(password: &str) -> Result<(CryptoBlock, Key)> {
    let mut block = CryptoBlock {
        m_cost_kib: M_COST_KIB,
        t_cost: T_COST,
        p_cost: P_COST,
        salt: [0; 16],
        wrapped: [0; 48],
    };
    random(&mut block.salt)?;
    let mut raw = [0u8; 32];
    random(&mut raw)?;
    let key = Key(raw);
    raw.zeroize();
    let kek = derive(password.as_bytes(), &block)?;
    // the wrapping nonce is fixed: the KEK is unique to this salt and used once
    let cipher = Aes256Gcm::new((&kek.0).into());
    let wrapped = cipher
        .encrypt(Nonce::from_slice(&[0u8; NONCE_LEN]), key.0.as_slice())
        .map_err(|_| Error::Archive("key wrapping failed".into()))?;
    block.wrapped.copy_from_slice(&wrapped);
    Ok((block, key))
}

/// Recover the archive key, or report a wrong password.
pub fn unlock(block: &CryptoBlock, password: &str) -> Result<Key> {
    let kek = derive(password.as_bytes(), block)?;
    let cipher = Aes256Gcm::new((&kek.0).into());
    let raw = cipher
        .decrypt(
            Nonce::from_slice(&[0u8; NONCE_LEN]),
            block.wrapped.as_slice(),
        )
        .map_err(|_| Error::WrongPassword)?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw);
    Ok(Key(key))
}

impl CryptoBlock {
    pub fn pack(&self) -> [u8; CRYPTO_SIZE] {
        let mut out = [0u8; CRYPTO_SIZE];
        out[0] = KDF_ARGON2ID;
        out[1..5].copy_from_slice(&self.m_cost_kib.to_le_bytes());
        out[5..9].copy_from_slice(&self.t_cost.to_le_bytes());
        out[9..13].copy_from_slice(&self.p_cost.to_le_bytes());
        out[13..29].copy_from_slice(&self.salt);
        out[29..77].copy_from_slice(&self.wrapped);
        let crc = crc32fast::hash(&out[..CRYPTO_SIZE - 4]);
        out[CRYPTO_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub fn parse(b: &[u8]) -> Result<CryptoBlock> {
        if b.len() != CRYPTO_SIZE {
            return Err(Error::Integrity("crypto block has the wrong size".into()));
        }
        let crc = u32::from_le_bytes(b[CRYPTO_SIZE - 4..].try_into().unwrap());
        if crc32fast::hash(&b[..CRYPTO_SIZE - 4]) != crc {
            return Err(Error::Integrity("crypto block CRC mismatch".into()));
        }
        if b[0] != KDF_ARGON2ID {
            return Err(Error::Archive(format!("unknown key derivation {}", b[0])));
        }
        let u = |p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
        let block = CryptoBlock {
            m_cost_kib: u(1),
            t_cost: u(5),
            p_cost: u(9),
            salt: b[13..29].try_into().unwrap(),
            wrapped: b[29..77].try_into().unwrap(),
        };
        if block.m_cost_kib < 8 * block.p_cost.max(1)
            || block.m_cost_kib > MAX_M_COST_KIB
            || block.t_cost == 0
            || block.t_cost > MAX_T_COST
            || block.p_cost == 0
            || block.p_cost > MAX_P_COST
        {
            return Err(Error::Integrity(
                "key derivation parameters are out of range".into(),
            ));
        }
        if b[77..CRYPTO_SIZE - 4].iter().any(|&x| x != 0) {
            return Err(Error::Integrity("crypto block has unexpected bytes".into()));
        }
        Ok(block)
    }
}

/// nonce || ciphertext || tag
pub fn seal(key: &Key, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let cipher = Aes256Gcm::new((&key.0).into());
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Archive("encryption failed".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &Key, data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if data.len() < OVERHEAD {
        return Err(Error::Integrity("encrypted block is too short".into()));
    }
    let cipher = Aes256Gcm::new((&key.0).into());
    cipher
        .decrypt(
            Nonce::from_slice(&data[..NONCE_LEN]),
            Payload {
                msg: &data[NONCE_LEN..],
                aad,
            },
        )
        .map_err(|_| Error::Integrity("encrypted data failed authentication".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_and_seal() {
        let (block, key) = new_block("hunter2").unwrap();
        let parsed = CryptoBlock::parse(&block.pack()).unwrap();
        assert_eq!(parsed, block);
        let again = unlock(&parsed, "hunter2").unwrap();
        assert_eq!(again.0, key.0);
        assert!(matches!(
            unlock(&parsed, "hunter3"),
            Err(Error::WrongPassword)
        ));
        let sealed = seal(&key, b"hello", b"aad").unwrap();
        assert_eq!(open(&key, &sealed, b"aad").unwrap(), b"hello");
        assert!(open(&key, &sealed, b"other").is_err());
        let mut bad = sealed.clone();
        bad[NONCE_LEN] ^= 1;
        assert!(open(&key, &bad, b"aad").is_err());
    }
}
