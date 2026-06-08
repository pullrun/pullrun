use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nimbus_exec::types::{Backend, WorkloadSpec};
use nimbus_exec::Executor;
use nimbus_oci::{
    build_dag_from_directory, DagDirectory, Dockerfile, Instruction, ManifestData,
    OciMaterializer, OciPuller,
};
use nimbus_store::{Digest, MmapStore};
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Errors that can occur during DAG-native image builds.
#[derive(Debug)]
pub enum BuildError {
    Parse(String),
    Pull(String),
    Materialize(String),
    Execute(String),
    Scan(String),
    Store(String),
    Config(String),
    Io(std::io::Error),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Parse(msg) => write!(f, "parse error: {msg}"),
            BuildError::Pull(msg) => write!(f, "pull error: {msg}"),
            BuildError::Materialize(msg) => write!(f, "materialize error: {msg}"),
            BuildError::Execute(msg) => write!(f, "execute error: {msg}"),
            BuildError::Scan(msg) => write!(f, "scan error: {msg}"),
            BuildError::Store(msg) => write!(f, "store error: {msg}"),
            BuildError::Config(msg) => write!(f, "config error: {msg}"),
            BuildError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<String> for BuildError {
    fn from(s: String) -> Self {
        BuildError::Parse(s)
    }
}

/// Result of a successful DAG-native build.
pub struct BuildResult {
    pub root_digest: Digest,
    pub layer_count: usize,
    pub node_count: usize,
    pub bytes_stored: u64,
}

/// DAG-native image builder. Orchestrates pulling base images, executing RUN
/// instructions, handling COPY, and snapshotting each layer into the DAG store.
pub struct DagBuilder {
    store: Arc<MmapStore>,
    runc_path: PathBuf,
    bundle_root: PathBuf,
    insecure_registries: std::collections::HashSet<String>,
    /// Track materialized rootfs paths per layer to reuse when possible.
    rootfs_cache: Arc<RwLock<HashMap<String, PathBuf>>>,
    /// Build layer cache: instruction_hash -> resulting_manifest_digest.
    /// Keyed by a content hash of the instruction + parent + inputs.
    /// A hit avoids re-executing the instruction.
    layer_cache: Arc<RwLock<HashMap<String, String>>>,
}

impl DagBuilder {
    pub fn new(
        store: Arc<MmapStore>,
        runc_path: PathBuf,
        bundle_root: PathBuf,
        insecure_registries: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            store,
            runc_path,
            bundle_root,
            insecure_registries,
            rootfs_cache: Arc::new(RwLock::new(HashMap::new())),
            layer_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build an image from a Dockerfile's first stage.
    /// Returns the final manifest digest.
    pub async fn build(
        &self,
        dockerfile: &Dockerfile,
        context_dir: &Path,
        build_args: &HashMap<String, String>,
    ) -> Result<BuildResult, BuildError> {
        let stage = &dockerfile.stages[0];
        self.build_stage(stage, context_dir, build_args).await
    }

    async fn build_stage(
        &self,
        stage: &nimbus_oci::BuildStage,
        context_dir: &Path,
        _build_args: &HashMap<String, String>,
    ) -> Result<BuildResult, BuildError> {
        // 1. Pull the base image
        info!("pulling base image: {}", stage.from);
        let puller = OciPuller::with_insecure_registries(
            None,
            self.insecure_registries.clone(),
        );
        let registry = extract_registry(&stage.from);
        let image_ref = extract_image_ref(&stage.from);
        let pulled = puller
            .pull(&image_ref, Some(registry.as_str()))
            .await
            .map_err(|e| BuildError::Pull(format!("{}: {e}", stage.from)))?;

        // 2. Convert to DAG
        let converter = nimbus_oci::OciToDagConverter::new(self.store.clone());
        let base_manifest = converter
            .convert(&pulled)
            .await
            .map_err(|e| BuildError::Pull(format!("convert {}: {e}", stage.from)))?;

        // 3. Read the manifest to get layer info
        let materializer = OciMaterializer::new(&self.store);
        let manifest_data = materializer
            .materialize_manifest(&base_manifest)
            .map_err(|e| BuildError::Pull(format!("manifest {}: {e}", stage.from)))?;

        // Track build metadata
        let mut current_manifest = base_manifest.clone();
        let mut manifest_data = manifest_data;
        let mut layer_count = 0usize;
        let mut total_nodes = 0usize;
        let mut total_bytes = 0u64;

        // 4. Execute each instruction (with layer caching)
        for instruction in &stage.instructions {
            match instruction {
                Instruction::Run { command } => {
                    let cache_key = self.cache_key_run(&current_manifest, command);
                    // Check cache
                    let cached = self.layer_cache.read().await.get(&cache_key).cloned();
                    if let Some(cached_digest) = cached {
                        info!("RUN {} [cached]", command.join(" "));
                        current_manifest = cached_digest;
                        layer_count += 1;
                        continue;
                    }
                    info!("RUN {}", command.join(" "));
                    current_manifest = self
                        .execute_run(&current_manifest, command, &mut manifest_data)
                        .await?;
                    // Store in cache
                    self.layer_cache
                        .write()
                        .await
                        .insert(cache_key, current_manifest.clone());
                    layer_count += 1;
                }
                Instruction::Copy { sources, dest } => {
                    let cache_key = self.cache_key_copy(&current_manifest, context_dir, sources, dest);
                    // Check cache
                    let cached = self.layer_cache.read().await.get(&cache_key).cloned();
                    if let Some(cached_digest) = cached {
                        info!("COPY {:?} {} [cached]", sources, dest);
                        current_manifest = cached_digest;
                        layer_count += 1;
                        continue;
                    }
                    info!("COPY {:?} {}", sources, dest);
                    current_manifest = self
                        .execute_copy(
                            &current_manifest,
                            context_dir,
                            sources,
                            dest,
                            &mut manifest_data,
                        )
                        .await?;
                    // Store in cache
                    self.layer_cache
                        .write()
                        .await
                        .insert(cache_key, current_manifest.clone());
                    layer_count += 1;
                }
                Instruction::Add { sources, dest } => {
                    let cache_key = self.cache_key_copy(&current_manifest, context_dir, sources, dest);
                    let cached = self.layer_cache.read().await.get(&cache_key).cloned();
                    if let Some(cached_digest) = cached {
                        info!("ADD {:?} {} [cached]", sources, dest);
                        current_manifest = cached_digest;
                        layer_count += 1;
                        continue;
                    }
                    info!("ADD {:?} {} (treated as COPY)", sources, dest);
                    current_manifest = self
                        .execute_copy(
                            &current_manifest,
                            context_dir,
                            sources,
                            dest,
                            &mut manifest_data,
                        )
                        .await?;
                    self.layer_cache
                        .write()
                        .await
                        .insert(cache_key, current_manifest.clone());
                    layer_count += 1;
                }
                Instruction::WorkDir(wd) => {
                    info!("WORKDIR {wd}");
                    manifest_data.working_dir = Some(wd.clone());
                }
                Instruction::Env { key, value } => {
                    info!("ENV {key}={value}");
                    manifest_data
                        .env
                        .push(format!("{key}={value}"));
                }
                Instruction::Cmd(cmd) => {
                    info!("CMD {:?}", cmd);
                    manifest_data.cmd = cmd.clone();
                }
                Instruction::Entrypoint(ep) => {
                    info!("ENTRYPOINT {:?}", ep);
                    manifest_data.entrypoint = ep.clone();
                }
                Instruction::Expose(_) => {
                    // Not tracked in v1 manifest_data
                }
                Instruction::Label { key, value } => {
                    info!("LABEL {key}={value}");
                    // Labels aren't in ManifestData yet; extend config_json
                }
                Instruction::User(user) => {
                    info!("USER {user}");
                    // Not tracked in v1 manifest_data
                }
                Instruction::Arg { name, .. } => {
                    debug!("ARG {name} (resolved at build time)");
                }
                Instruction::Shell(shell) => {
                    info!("SHELL {:?}", shell);
                    // Not tracked in v1 manifest_data
                }
                Instruction::Comment(_) => {}
            }
        }

        // 5. Compute final stats
        let dig: Digest = current_manifest
            .parse()
            .map_err(|e| BuildError::Store(format!("invalid digest {current_manifest}: {e}")))?;
        if let Ok(layers) = self.store.get_deserialized(&dig) {
            total_nodes += 1; // manifest
            for edge in &layers.edges {
                total_nodes += 1; // layer
                if let Ok(layer) = self.store.get_deserialized(edge) {
                    total_nodes += layer.edges.len(); // trees and blobs under layer
                }
            }
        }

        // 6. Create a new manifest node with the final layers merged
        let final_manifest = self
            .update_manifest(&current_manifest, &manifest_data)
            .await?;

        Ok(BuildResult {
            root_digest: final_manifest,
            layer_count,
            node_count: total_nodes,
            bytes_stored: total_bytes,
        })
    }

    /// Execute a RUN instruction: materialize, run in container, re-scan.
    async fn execute_run(
        &self,
        current_manifest: &str,
        command: &[String],
        _manifest_data: &mut ManifestData,
    ) -> Result<String, BuildError> {
        let temp_dir = tempfile::tempdir()
            .map_err(BuildError::Io)?;
        let rootfs_dir = temp_dir.path().join("rootfs");

        // Materialize current state
        let dig: Digest = current_manifest
            .parse()
            .map_err(|e| BuildError::Store(format!("bad digest: {e}")))?;
        let materializer = OciMaterializer::new(&self.store);
        materializer
            .materialize_into(&dig, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Materialize(e.to_string()))?;

        // Run the command in a runc container using the materialized rootfs
        let workload_id = format!(
            "build-run-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let bundle_dir = self.bundle_root.join(&workload_id);
        std::fs::create_dir_all(&bundle_dir)
            .map_err(BuildError::Io)?;

        // Build OCI config.json
        let config = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": {
                "args": command,
                "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "cwd": "/",
                "capabilities": {
                    "bounding": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE", "CAP_NET_RAW", "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_AUDIT_WRITE", "CAP_SETFCAP"],
                    "effective": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE", "CAP_NET_RAW", "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_AUDIT_WRITE", "CAP_SETFCAP"],
                    "permitted": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE", "CAP_NET_RAW", "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_AUDIT_WRITE", "CAP_SETFCAP"]
                },
                "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 1024}]
            },
            "root": {
                "path": rootfs_dir.to_string_lossy(),
                "readonly": false
            },
            "hostname": "nimbus-build",
            "mounts": [
                {"destination": "/proc", "type": "proc", "source": "proc"},
                {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
                {"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]},
                {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev", "ro"]},
                {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777"]},
                {"destination": "/dev/shm", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777"]}
            ],
            "linux": {
                "namespaces": [
                    {"type": "pid"},
                    {"type": "ipc"},
                    {"type": "uts"},
                    {"type": "mount"},
                    {"type": "network"}
                ]
            }
        });

        let config_path = bundle_dir.join("config.json");
        let config_json = serde_json::to_string_pretty(&config)
            .map_err(|e| BuildError::Config(e.to_string()))?;
        std::fs::write(&config_path, &config_json)
            .map_err(BuildError::Io)?;

        // Write /etc/hosts and /etc/resolv.conf so entrypoint scripts
        // that expect these files don't fail.
        let etc_dir = rootfs_dir.join("etc");
        let _ = std::fs::create_dir_all(&etc_dir);
        let hosts_path = etc_dir.join("hosts");
        if !hosts_path.exists() {
            std::fs::write(
                &hosts_path,
                "127.0.0.1 localhost\n::1 localhost ip6-localhost\n",
            )
            .ok();
        }
        let resolv_path = etc_dir.join("resolv.conf");
        if !resolv_path.exists() {
            std::fs::write(&resolv_path, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n")
                .ok();
        }

        // Symlink rootfs into bundle
        let bundle_rootfs = bundle_dir.join("rootfs");
        if bundle_rootfs.exists() {
            std::fs::remove_dir_all(&bundle_rootfs).ok();
        }
        std::os::unix::fs::symlink(&rootfs_dir, &bundle_rootfs)
            .map_err(BuildError::Io)?;

        // Run runc
        let output = std::process::Command::new(&self.runc_path)
            .args(["run", &workload_id])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(&bundle_dir)
            .output()
            .map_err(|e| BuildError::Execute(format!("spawn runc: {e}")))?;

        // Clean up bundle
        std::fs::remove_dir_all(&bundle_dir).ok();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BuildError::Execute(format!(
                "command {:?} failed: {}\nstderr: {stderr}",
                command,
                output.status
            )));
        }

        // Re-scan the rootfs into DAG nodes
        info!("scanning rootfs after RUN...");
        let DagDirectory {
            manifest_digest,
            node_count,
            blob_bytes,
        } = build_dag_from_directory(&self.store, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Scan(e.to_string()))?;

        debug!("RUN layer: {node_count} nodes, {blob_bytes} bytes");
        Ok(manifest_digest)
    }

    /// Execute a COPY instruction: copy files from context into rootfs, re-scan.
    async fn execute_copy(
        &self,
        current_manifest: &str,
        context_dir: &Path,
        sources: &[String],
        dest: &str,
        _manifest_data: &mut ManifestData,
    ) -> Result<String, BuildError> {
        let temp_dir = tempfile::tempdir()
            .map_err(BuildError::Io)?;
        let rootfs_dir = temp_dir.path().join("rootfs");

        // Materialize current state
        let dig: Digest = current_manifest
            .parse()
            .map_err(|e| BuildError::Store(format!("bad digest: {e}")))?;
        let materializer = OciMaterializer::new(&self.store);
        materializer
            .materialize_into(&dig, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Materialize(e.to_string()))?;

        // Resolve destination path
        let dest_path = if dest.starts_with('/') {
            rootfs_dir.join(dest.strip_prefix('/').unwrap_or(dest))
        } else {
            rootfs_dir.join(dest)
        };
        std::fs::create_dir_all(&dest_path)
            .map_err(BuildError::Io)?;

        // Copy each source into the rootfs
        for src in sources {
            let src_path = context_dir.join(src);
            let dest_path = dest_path.join(
                src.rsplit('/').next().unwrap_or(src),
            );
            copy_recursive(&src_path, &dest_path)
                .map_err(|e| BuildError::Io(e))?;
        }

        // Re-scan the rootfs into DAG nodes
        info!("scanning rootfs after COPY...");
        let DagDirectory {
            manifest_digest,
            node_count,
            blob_bytes,
        } = build_dag_from_directory(&self.store, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Scan(e.to_string()))?;

        debug!("COPY layer: {node_count} nodes, {blob_bytes} bytes");
        Ok(manifest_digest)
    }

    /// Update the manifest node's config data (entrypoint/cmd/env/etc).
    async fn update_manifest(
        &self,
        current_manifest: &str,
        manifest_data: &ManifestData,
    ) -> Result<String, BuildError> {
        let dig: Digest = current_manifest
            .parse()
            .map_err(|e| BuildError::Store(format!("bad digest: {e}")))?;
        if let Ok(node) = self.store.get_deserialized(&dig) {
            let inline = serde_json::to_vec(manifest_data).unwrap_or_default();
            let new_node = nimbus_store::DagNode::manifest(node.edges.clone(), inline);
            let new_dig = self
                .store
                .put_blocking(&new_node)
                .map_err(|e| BuildError::Store(e.to_string()))?;
            Ok(new_dig)
        } else {
            Err(BuildError::Store(format!(
                "manifest {current_manifest} not found"
            )))
        }
    }

    // -----------------------------------------------------------------------
    // Layer caching helpers
    // -----------------------------------------------------------------------

    /// Compute a cache key for a RUN instruction.
    /// Includes the parent manifest digest and the full command so that
    /// changing either invalidates the cache.
    fn cache_key_run(&self, parent_manifest: &str, command: &[String]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"RUN:");
        hasher.update(parent_manifest.as_bytes());
        for arg in command {
            hasher.update(b"\0");
            hasher.update(arg.as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    /// Compute a cache key for a COPY instruction.
    /// Includes the parent manifest digest, source file paths, destination,
    /// and the content hash of each source file. Changing any input file
    /// content invalidates the cache.
    fn cache_key_copy(
        &self,
        parent_manifest: &str,
        context_dir: &Path,
        sources: &[String],
        dest: &str,
    ) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"COPY:");
        hasher.update(parent_manifest.as_bytes());
        hasher.update(b":");
        hasher.update(dest.as_bytes());
        for src in sources {
            hasher.update(b"\0");
            hasher.update(src.as_bytes());
            // Mix in the content hash of each source file.
            let src_path = context_dir.join(src);
            if let Ok(content) = std::fs::read(&src_path) {
                let mut file_hasher = Sha256::new();
                file_hasher.update(&content);
                let file_hash = hex::encode(file_hasher.finalize());
                hasher.update(b":");
                hasher.update(file_hash.as_bytes());
            }
        }
        hex::encode(hasher.finalize())
    }
}

fn extract_registry(image_ref: &str) -> String {
    let parts: Vec<&str> = image_ref.splitn(2, '/').collect();
    if parts.len() == 2
        && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost")
    {
        parts[0].to_string()
    } else {
        "docker.io".to_string()
    }
}

fn extract_image_ref(image_ref: &str) -> String {
    let parts: Vec<&str> = image_ref.splitn(2, '/').collect();
    if parts.len() == 2
        && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost")
    {
        parts[1..].join("/")
    } else {
        image_ref.to_string()
    }
}

fn copy_recursive(src: &Path, dest: &Path) -> Result<(), std::io::Error> {
    if src.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_src = entry.path();
            let child_dest = dest.join(entry.file_name());
            copy_recursive(&child_src, &child_dest)?;
        }
        Ok(())
    } else if src.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dest)?;
        Ok(())
    } else {
        std::fs::copy(src, dest)?;
        Ok(())
    }
}
