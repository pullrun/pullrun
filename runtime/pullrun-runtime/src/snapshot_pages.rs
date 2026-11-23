// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Content-addressed guest-memory pages for snapshot dedup.
//!
//! A paged memory snapshot stores a `PRPAGE1\0` index of SHA-256 page
//! hashes; each page lives once under `<store>/snapshot-pages/<hash>`.
//! Pages are AES-256-GCM encrypted with a nonce derived from the hash
//! (convergent encryption), so identical pages dedup and still are not
//! stored in plaintext.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"PRPAGE1\0";
pub const PAGE_SIZE: usize = 64 * 1024;
const HASH_LEN: usize = 32;

fn page_path(store_root: &Path, hash: &[u8]) -> std::path::PathBuf {
    store_root.join("snapshot-pages").join(hex(hash))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encrypt_page(plain: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    let hash = Sha256::digest(plain);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key: {e}"))?;
    let nonce = &hash[..12];
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), plain)
        .map_err(|e| format!("encrypt page: {e}"))?;
    Ok(ct)
}

fn decrypt_page(ciphertext: &[u8], key: &[u8; 32], hash: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key: {e}"))?;
    cipher
        .decrypt(Nonce::from_slice(&hash[..12]), ciphertext)
        .map_err(|_| "decrypt page: authentication failed".to_string())
}

/// Replace `mem_path` with a paged index, storing unique pages.
pub fn store(mem_path: &Path, store_root: &Path, key: &[u8; 32]) -> Result<(), String> {
    let mut plain = Vec::new();
    std::fs::File::open(mem_path)
        .and_then(|mut f| f.read_to_end(&mut plain))
        .map_err(|e| format!("read {}: {e}", mem_path.display()))?;
    let pages_dir = store_root.join("snapshot-pages");
    std::fs::create_dir_all(&pages_dir).map_err(|e| format!("pages dir: {e}"))?;
    let mut hashes = Vec::new();
    for chunk in plain.chunks(PAGE_SIZE) {
        let hash = Sha256::digest(chunk);
        let path = page_path(store_root, &hash);
        if !path.exists() {
            let ct = encrypt_page(chunk, key)?;
            let tmp = path.with_extension("tmp");
            let mut f = std::fs::File::create(&tmp).map_err(|e| format!("page tmp: {e}"))?;
            f.write_all(&ct)
                .and_then(|_| f.sync_all())
                .map_err(|e| format!("page write: {e}"))?;
            std::fs::rename(&tmp, &path).map_err(|e| format!("page rename: {e}"))?;
        }
        hashes.extend_from_slice(&hash);
    }
    let count = hashes.len() / HASH_LEN;
    let mut index = Vec::with_capacity(MAGIC.len() + 8 + hashes.len());
    index.extend_from_slice(MAGIC);
    index.extend_from_slice(&(count as u64).to_le_bytes());
    index.extend_from_slice(&hashes);
    let tmp = mem_path.with_extension("page.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("index tmp: {e}"))?;
        f.write_all(&index)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("index write: {e}"))?;
    }
    std::fs::rename(&tmp, mem_path).map_err(|e| format!("index rename: {e}"))
}

#[allow(dead_code)]
pub fn is_paged(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).is_ok() && &magic == MAGIC
}

/// Page hashes referenced by a paged memory index.
pub fn page_hashes(index_path: &Path) -> Result<Vec<Vec<u8>>, String> {
    let bytes = std::fs::read(index_path).map_err(|e| format!("read index: {e}"))?;
    if bytes.len() < MAGIC.len() + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err("not a paged snapshot".into());
    }
    let count = u64::from_le_bytes(
        bytes[MAGIC.len()..MAGIC.len() + 8]
            .try_into()
            .map_err(|_| "corrupt page index")?,
    ) as usize;
    let hashes = &bytes[MAGIC.len() + 8..];
    if hashes.len() != count * HASH_LEN {
        return Err("corrupt page index".into());
    }
    Ok(hashes.chunks_exact(HASH_LEN).map(|h| h.to_vec()).collect())
}

/// Rebuild a raw guest-memory file from a paged index.
pub fn reconstruct(
    index_path: &Path,
    store_root: &Path,
    key: &[u8; 32],
    dst: &Path,
) -> Result<(), String> {
    let bytes = std::fs::read(index_path).map_err(|e| format!("read index: {e}"))?;
    if bytes.len() < MAGIC.len() + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err("not a paged snapshot".into());
    }
    let count =
        u64::from_le_bytes(bytes[MAGIC.len()..MAGIC.len() + 8].try_into().unwrap()) as usize;
    let hashes = &bytes[MAGIC.len() + 8..];
    if hashes.len() != count * HASH_LEN {
        return Err("corrupt page index".into());
    }
    let mut out = std::fs::File::create(dst).map_err(|e| format!("create mem: {e}"))?;
    for hash in hashes.chunks_exact(HASH_LEN) {
        let ct = std::fs::read(page_path(store_root, hash))
            .map_err(|e| format!("read page {}: {e}", hex(hash)))?;
        let plain = decrypt_page(&ct, key, hash)?;
        out.write_all(&plain)
            .map_err(|e| format!("write mem: {e}"))?;
    }
    out.sync_all().map_err(|e| format!("sync mem: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_identical_pages_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let key = crate::snapshot_crypto::load_or_create_key(&root).unwrap();
        let mem = dir.path().join("mem");
        let page = vec![7u8; PAGE_SIZE];
        let mut raw = page.clone();
        raw.extend_from_slice(&page);
        raw.extend_from_slice(b"tail");
        std::fs::write(&mem, &raw).unwrap();

        store(&mem, &root, &key).unwrap();
        assert!(is_paged(&mem));
        let pages = std::fs::read_dir(root.join("snapshot-pages"))
            .unwrap()
            .count();
        assert_eq!(pages, 2, "two identical pages + tail");

        let out = dir.path().join("mem.raw");
        reconstruct(&mem, &root, &key, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), raw);
    }

    #[test]
    fn tampered_page_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let key = crate::snapshot_crypto::load_or_create_key(&root).unwrap();
        let mem = dir.path().join("mem");
        std::fs::write(&mem, vec![1u8; PAGE_SIZE]).unwrap();
        store(&mem, &root, &key).unwrap();

        let entry = std::fs::read_dir(root.join("snapshot-pages"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut bytes = std::fs::read(&entry).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&entry, bytes).unwrap();
        assert!(reconstruct(&mem, &root, &key, &dir.path().join("out")).is_err());
    }
}
