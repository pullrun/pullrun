use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An operation lock file whose existence signals 'operation in progress'
/// to the garbage collector. GC checks for active op locks and defers
/// if any are found.
///
/// The lock file is automatically removed when the `OpLock` is dropped
/// (RAII pattern), covering both success and error paths without manual
/// cleanup calls.
///
/// v1: Lock existence alone is the signal — per-digest registration
/// (append-then-put) is deferred to v2.
///
/// File format (v2): one hex digest per line, UTF-8.
/// Stale lock detection: if file mtime is older than 1h, it's stale.
/// Heartbeat: `lock.touch()` refreshes the mtime.
pub struct OpLock {
    path: PathBuf,
    op_id: String,
}

impl OpLock {
    /// Create a new op lock under `<store_root>/ops/<op_id>`.
    pub fn new(store_root: &Path) -> std::io::Result<Self> {
        let ops_dir = store_root.join("ops");
        fs::create_dir_all(&ops_dir)?;
        let op_id = generate_op_id();
        let path = ops_dir.join(&op_id);
        {
            let file = fs::File::create(&path)?;
            file.sync_all()?;
        }
        Ok(Self { path, op_id })
    }

    /// Append a digest hex string to the lock file and fsync.
    pub fn append(&self, digest_hex: &str) -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(file, "{digest_hex}")?;
        file.sync_all()?;
        Ok(())
    }

    /// Heartbeat: update the file's mtime to now.
    /// Call after each operation step to keep the lock alive.
    pub fn touch(&self) -> std::io::Result<()> {
        // set_modified is stable on Unix/macOS
        let file = fs::File::open(&self.path)?;
        file.set_modified(SystemTime::now())?;
        Ok(())
    }

    /// Remove the lock file. Idempotent — does nothing if already removed.
    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }

    /// The unique ID for this lock (used as the filename).
    pub fn op_id(&self) -> &str {
        &self.op_id
    }

    /// The path to the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consume the lock, removing the file immediately.
    /// Call on the success path to release the lock early.
    /// On error paths, just let the lock go out of scope — Drop handles it.
    pub fn complete(self) {
        // Drop runs here and removes the file
    }
}

impl Drop for OpLock {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Read all digests from an op lock file. Returns the hex digests.
pub fn read_op_lock(path: &Path) -> std::io::Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Check whether an op lock is stale (mtime older than 1 hour).
pub fn is_op_lock_stale(path: &Path) -> std::io::Result<bool> {
    let meta = fs::metadata(path)?;
    let modified = meta.modified()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    Ok(age > Duration::from_secs(3600))
}

/// Clean up stale op lock files in `<store_root>/ops/`.
/// Returns the list of (op_id, digests) for locks that are still fresh.
pub fn clean_stale_op_locks(store_root: &Path) -> std::io::Result<Vec<(String, Vec<String>)>> {
    let ops_dir = store_root.join("ops");
    if !ops_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut fresh = Vec::new();
    for entry in fs::read_dir(&ops_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let op_id = entry.file_name().to_string_lossy().into_owned();
            if is_op_lock_stale(&path)? {
                let _ = fs::remove_file(&path);
            } else {
                let digests = read_op_lock(&path).unwrap_or_default();
                fresh.push((op_id, digests));
            }
        }
    }
    Ok(fresh)
}

fn generate_op_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("op_{nanos:020x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_create_and_append() {
        let dir = tempdir().unwrap();
        let lock = OpLock::new(dir.path()).unwrap();
        assert!(lock.path().exists());

        lock.append("abc123").unwrap();
        lock.append("def456").unwrap();

        let digests = read_op_lock(lock.path()).unwrap();
        assert_eq!(digests, vec!["abc123", "def456"]);
    }

    #[test]
    fn test_remove() {
        let dir = tempdir().unwrap();
        let lock = OpLock::new(dir.path()).unwrap();
        assert!(lock.path().exists());
        lock.remove();
        assert!(!lock.path().exists());
        // Idempotent
        lock.remove();
    }

    #[test]
    fn test_touch_refreshes_mtime() {
        let dir = tempdir().unwrap();
        let lock = OpLock::new(dir.path()).unwrap();
        let original = lock.path().metadata().unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(10));
        lock.touch().unwrap();

        let updated = lock.path().metadata().unwrap().modified().unwrap();
        assert!(updated >= original);
    }

    #[test]
    fn test_stale_detection() {
        let dir = tempdir().unwrap();
        let lock = OpLock::new(dir.path()).unwrap();
        assert!(!is_op_lock_stale(lock.path()).unwrap());
    }

    #[test]
    fn test_op_id_is_non_empty() {
        let dir = tempdir().unwrap();
        let lock = OpLock::new(dir.path()).unwrap();
        assert!(!lock.op_id().is_empty());
    }

    #[test]
    fn test_clean_stale_op_locks() {
        let dir = tempdir().unwrap();

        // Create one fresh lock and one stale lock
        let fresh = OpLock::new(dir.path()).unwrap();
        fresh.append("abc").unwrap();

        let stale_path = dir.path().join("ops").join("stale_test");
        {
            let mut f = fs::File::create(&stale_path).unwrap();
            writeln!(f, "stale_digest").unwrap();
            f.sync_all().unwrap();
        }
        // Set mtime to 2 hours ago
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        let file = fs::File::open(&stale_path).unwrap();
        file.set_modified(two_hours_ago).unwrap();
        drop(file);

        let fresh_locks = clean_stale_op_locks(dir.path()).unwrap();
        assert_eq!(fresh_locks.len(), 1);
        assert_eq!(fresh_locks[0].1, vec!["abc"]);
        // Stale file should be removed
        assert!(!stale_path.exists());
    }
}
