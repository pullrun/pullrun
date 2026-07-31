// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use tracing::{debug, info};

const SECRET_KEY_FILE: &str = "secret.key";
const SECRETS_DIR: &str = "secrets";
const CONFIGS_DIR: &str = "configs";
const NONCE_LEN: usize = 12;

#[derive(Clone, Debug)]
pub struct SecretInfo {
    pub name: String,
    pub created_at: i64,
    pub size_bytes: i64,
}

#[derive(Debug)]
pub struct SecretStore {
    store_root: PathBuf,
    key_cache: Mutex<Option<[u8; 32]>>,
}

impl Clone for SecretStore {
    fn clone(&self) -> Self {
        Self {
            store_root: self.store_root.clone(),
            key_cache: Mutex::new(*self.key_cache.lock().expect("key_cache lock poisoned")),
        }
    }
}

impl SecretStore {
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store_root,
            key_cache: Mutex::new(None),
        }
    }

    fn secrets_dir(&self) -> PathBuf {
        self.store_root.join(SECRETS_DIR)
    }

    fn configs_dir(&self) -> PathBuf {
        self.store_root.join(CONFIGS_DIR)
    }

    fn key_path(&self) -> PathBuf {
        self.store_root.join(SECRET_KEY_FILE)
    }

    fn sanitize_name(name: &str) -> Result<String, &'static str> {
        if name.is_empty() || name.len() > 255 {
            return Err("name must be 1-255 characters");
        }
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err("name must be alphanumeric with dashes, underscores, or dots");
        }
        if name.starts_with('.') || name.starts_with('-') {
            return Err("name must not start with dot or dash");
        }
        Ok(name.to_string())
    }

    fn load_or_generate_key(&self) -> Result<[u8; 32], String> {
        let mut cache = self.key_cache.lock().expect("key_cache lock poisoned");
        if let Some(key) = *cache {
            return Ok(key);
        }
        let key_path = self.key_path();
        let key = if key_path.exists() {
            let data = std::fs::read(&key_path).map_err(|e| format!("read key: {e}"))?;
            if data.len() != 32 {
                return Err(format!("key file has wrong length: {}", data.len()));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&data);
            key
        } else {
            let mut key = [0u8; 32];
            OsRng.fill_bytes(&mut key);
            if let Some(parent) = key_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create key dir: {e}"))?;
            }
            // Write with 0600 from the start (mode on the file as it is
            // created), then fsync so a crash cannot leave a truncated
            // key that permanently locks every secret.
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            opts.mode(0o600);
            let mut f = opts
                .open(&key_path)
                .map_err(|e| format!("write key: {e}"))?;
            f.write_all(&key).map_err(|e| format!("write key: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync key: {e}"))?;
            info!("generated new secret key at {}", key_path.display());
            key
        };
        *cache = Some(key);
        Ok(key)
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let key = self.load_or_generate_key()?;
        let cipher =
            Aes256Gcm::new_from_slice(key.as_slice()).map_err(|e| format!("cipher init: {e}"))?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|e| format!("encrypt: {e}"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        if data.len() < NONCE_LEN {
            return Err("data too short".to_string());
        }
        let key = self.load_or_generate_key()?;
        let cipher =
            Aes256Gcm::new_from_slice(key.as_slice()).map_err(|e| format!("cipher init: {e}"))?;
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|e| format!("decrypt: {e}"))
    }

    pub fn create_secret(&self, name: &str, data: &[u8]) -> Result<(), String> {
        let name = Self::sanitize_name(name)?;
        let path = self.secrets_dir().join(&name);
        std::fs::create_dir_all(self.secrets_dir())
            .map_err(|e| format!("create secrets dir: {e}"))?;
        let encrypted = self.encrypt(data)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        opts.mode(0o600);
        let mut file = opts.open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("secret '{name}' already exists")
            } else {
                format!("create secret '{name}': {e}")
            }
        })?;
        std::io::Write::write_all(&mut file, &encrypted)
            .map_err(|e| format!("write secret '{name}': {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync secret '{name}': {e}"))?;
        debug!(%name, "secret created");
        Ok(())
    }

    pub fn read_secret(&self, name: &str) -> Result<String, String> {
        let name = Self::sanitize_name(name)?;
        let path = self.secrets_dir().join(&name);
        let encrypted = std::fs::read(&path).map_err(|e| format!("read secret '{name}': {e}"))?;
        let plaintext = self.decrypt(&encrypted)?;
        String::from_utf8(plaintext).map_err(|e| format!("secret '{name}' is not valid UTF-8: {e}"))
    }

    pub fn read_secret_raw(&self, name: &str) -> Result<Vec<u8>, String> {
        let name = Self::sanitize_name(name)?;
        let path = self.secrets_dir().join(&name);
        let encrypted = std::fs::read(&path).map_err(|e| format!("read secret '{name}': {e}"))?;
        self.decrypt(&encrypted)
    }

    pub fn list_secrets(&self) -> Result<Vec<SecretInfo>, String> {
        let dir = self.secrets_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("list secrets: {e}"))? {
            let entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    out.push(SecretInfo {
                        name,
                        created_at: timestamp_from_meta(&meta),
                        size_bytes: meta.len() as i64,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn inspect_secret(&self, name: &str) -> Result<SecretInfo, String> {
        let name_clean = Self::sanitize_name(name)?;
        let path = self.secrets_dir().join(&name_clean);
        let meta = std::fs::metadata(&path).map_err(|e| format!("inspect secret '{name}': {e}"))?;
        if !meta.is_file() {
            return Err(format!("'{}' is not a file", name));
        }
        Ok(SecretInfo {
            name: name_clean,
            created_at: timestamp_from_meta(&meta),
            size_bytes: meta.len() as i64,
        })
    }

    pub fn remove_secret(&self, name: &str) -> Result<(), String> {
        let name = Self::sanitize_name(name)?;
        let path = self.secrets_dir().join(&name);
        if !path.exists() {
            return Err(format!("secret '{name}' not found"));
        }
        std::fs::remove_file(&path).map_err(|e| format!("remove secret '{name}': {e}"))?;
        debug!(%name, "secret removed");
        Ok(())
    }

    // ─── Configs (unencrypted) ────────────────────────────────

    pub fn create_config(&self, name: &str, data: &[u8]) -> Result<(), String> {
        let name = Self::sanitize_name(name)?;
        let path = self.configs_dir().join(&name);
        std::fs::create_dir_all(self.configs_dir())
            .map_err(|e| format!("create configs dir: {e}"))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!("config '{name}' already exists")
                } else {
                    format!("create config '{name}': {e}")
                }
            })?;
        std::io::Write::write_all(&mut file, data)
            .map_err(|e| format!("write config '{name}': {e}"))?;
        debug!(%name, "config created");
        Ok(())
    }

    pub fn read_config(&self, name: &str) -> Result<String, String> {
        let name = Self::sanitize_name(name)?;
        let path = self.configs_dir().join(&name);
        std::fs::read_to_string(&path).map_err(|e| format!("read config '{name}': {e}"))
    }

    pub fn read_config_raw(&self, name: &str) -> Result<Vec<u8>, String> {
        let name = Self::sanitize_name(name)?;
        let path = self.configs_dir().join(&name);
        std::fs::read(&path).map_err(|e| format!("read config '{name}': {e}"))
    }

    pub fn list_configs(&self) -> Result<Vec<SecretInfo>, String> {
        let dir = self.configs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("list configs: {e}"))? {
            let entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    out.push(SecretInfo {
                        name,
                        created_at: timestamp_from_meta(&meta),
                        size_bytes: meta.len() as i64,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn inspect_config(&self, name: &str) -> Result<SecretInfo, String> {
        let name_clean = Self::sanitize_name(name)?;
        let path = self.configs_dir().join(&name_clean);
        let meta = std::fs::metadata(&path).map_err(|e| format!("inspect config '{name}': {e}"))?;
        if !meta.is_file() {
            return Err(format!("'{}' is not a file", name));
        }
        Ok(SecretInfo {
            name: name_clean,
            created_at: timestamp_from_meta(&meta),
            size_bytes: meta.len() as i64,
        })
    }

    pub fn remove_config(&self, name: &str) -> Result<(), String> {
        let name = Self::sanitize_name(name)?;
        let path = self.configs_dir().join(&name);
        if !path.exists() {
            return Err(format!("config '{name}' not found"));
        }
        std::fs::remove_file(&path).map_err(|e| format!("remove config '{name}': {e}"))?;
        debug!(%name, "config removed");
        Ok(())
    }
}

// ─── Helpers for container mount staging ─────────────────────
//
/// Try to extract a Unix-epoch timestamp from filesystem metadata.
/// Falls back to `modified()` when `created()` is unavailable (Linux
/// ext4/xfs/btrfs), then to `0` if neither is available.
fn timestamp_from_meta(meta: &std::fs::Metadata) -> i64 {
    let ts = meta.created().ok().or_else(|| meta.modified().ok());
    ts.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// When a container is run with --secret or --config, the runtime
// resolves the content and writes it into the bundle's secret staging
// directory.  These files are then bind-mounted into the container.

/// Write a resolved secret into the bundle's staging directory.
/// Returns (source_path, target_path) for the bind mount.
/// target defaults to `/run/secrets/<name>`.
///
/// The staging directory and files are created 0700/0600 — they hold
/// plaintext secrets and are bind-mounted into running workloads.
pub fn stage_secret(
    store_root: &Path,
    bundle_id: &str,
    secret_content: &[u8],
    secret_name: &str,
    custom_target: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let target = match custom_target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => format!("/run/secrets/{}", secret_name),
    };
    let dest_dir = store_root
        .join("bundles")
        .join(bundle_id)
        .join("run")
        .join("secrets");
    create_stage_dir(&dest_dir).map_err(|e| format!("create secret dir: {e}"))?;
    let file_path = dest_dir.join(secret_name);
    write_stage_file(&file_path, secret_content)
        .map_err(|e| format!("write staged secret: {e}"))?;
    Ok((file_path, target))
}

/// Write a resolved config into the bundle's staging directory.
/// Returns (source_path, target_path) for the bind mount.
/// target defaults to `/<name>`.
pub fn stage_config(
    store_root: &Path,
    bundle_id: &str,
    config_content: &[u8],
    config_name: &str,
    custom_target: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let target = match custom_target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => format!("/{}", config_name),
    };
    let dest_dir = store_root
        .join("bundles")
        .join(bundle_id)
        .join("run")
        .join("secrets");
    create_stage_dir(&dest_dir).map_err(|e| format!("create config dir: {e}"))?;
    let file_path = dest_dir.join(config_name);
    write_stage_file(&file_path, config_content)
        .map_err(|e| format!("write staged config: {e}"))?;
    Ok((file_path, target))
}

fn create_stage_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

fn write_stage_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    opts.mode(0o600);
    let mut f = opts.open(path)?;
    std::io::Write::write_all(&mut f, content)?;
    f.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
