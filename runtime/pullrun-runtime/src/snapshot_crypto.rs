// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! At-rest encryption for snapshot state/memory/rootfs files.
//!
//! Format: `PRSNAP1\0` magic || 12-byte nonce || AES-256-GCM ciphertext.
//! The key lives in `<store>/snapshots.key` with mode 0600.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAGIC: &[u8; 8] = b"PRSNAP1\0";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

pub fn load_or_create_key(store_root: &Path) -> std::io::Result<[u8; KEY_LEN]> {
    let path = store_root.join("snapshots.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == KEY_LEN {
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let mut key = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut key);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(&key)?;
            f.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return load_or_create_key(store_root);
        }
        Err(e) => return Err(e),
    }
    Ok(key)
}

pub fn is_encrypted(path: &Path) -> bool {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).is_ok() && &magic == MAGIC
}

pub fn encrypt_file(path: &Path, key: &[u8; KEY_LEN]) -> Result<(), String> {
    let mut plain = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut plain))
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key: {e}"))?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_ref())
        .map_err(|e| format!("encrypt {}: {e}", path.display()))?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    let tmp = path.with_extension("enc.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&out)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

pub fn decrypt_file(src: &Path, dst: &Path, key: &[u8; KEY_LEN]) -> Result<(), String> {
    let bytes = std::fs::read(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    if bytes.len() < MAGIC.len() + NONCE_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(format!("{} is not an encrypted snapshot", src.display()));
    }
    let nonce = &bytes[MAGIC.len()..MAGIC.len() + NONCE_LEN];
    let ciphertext = &bytes[MAGIC.len() + NONCE_LEN..];
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key: {e}"))?;
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| format!("decrypt {}: authentication failed", src.display()))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dst)
        .map_err(|e| format!("create {}: {e}", dst.display()))?;
    f.write_all(&plain)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {e}", dst.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let key = load_or_create_key(store).unwrap();
        let src = store.join("mem");
        std::fs::write(&src, b"guest-memory").unwrap();
        encrypt_file(&src, &key).unwrap();
        assert!(is_encrypted(&src));

        let out = store.join("mem.out");
        decrypt_file(&src, &out, &key).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"guest-memory");

        let mut bytes = std::fs::read(&src).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&src, bytes).unwrap();
        assert!(decrypt_file(&src, &out, &key).is_err());
    }

    #[test]
    fn key_is_created_0600_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let k1 = load_or_create_key(dir.path()).unwrap();
        let k2 = load_or_create_key(dir.path()).unwrap();
        assert_eq!(k1, k2);
        let mode = std::fs::metadata(dir.path().join("snapshots.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
