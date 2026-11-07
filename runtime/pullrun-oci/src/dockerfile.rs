// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pullrun_store::{DagNode, Digest, MmapStore, SMALL_FILE_THRESHOLD};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::OciError;

// ---------------------------------------------------------------------------
// Dockerfile parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Instruction {
    Run {
        command: Vec<String>,
    },
    Copy {
        sources: Vec<String>,
        dest: String,
    },
    WorkDir(String),
    Env {
        key: String,
        value: String,
    },
    Cmd(Vec<String>),
    Entrypoint(Vec<String>),
    Expose(u16),
    Label {
        key: String,
        value: String,
    },
    User(String),
    Arg {
        name: String,
        default: Option<String>,
    },
    Shell(Vec<String>),
    Add {
        sources: Vec<String>,
        dest: String,
    },
    Comment(String),
}

#[derive(Debug, Clone)]
pub struct BuildStage {
    pub from: String,
    pub instructions: Vec<Instruction>,
    pub name: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug)]
pub struct Dockerfile {
    pub stages: Vec<BuildStage>,
}

impl Dockerfile {
    pub fn parse(content: &str) -> Result<Self, String> {
        let lines = preprocess_lines(content);
        let blocks = split_into_blocks(&lines);
        let stages = blocks
            .into_iter()
            .map(parse_block)
            .collect::<Result<Vec<_>, _>>()?;
        if stages.is_empty() {
            return Err("Dockerfile has no stages".to_string());
        }
        Ok(Dockerfile { stages })
    }
}

fn preprocess_lines(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            if !current.is_empty() {
                out.push(current.trim().to_string());
                current = String::new();
            }
            continue;
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
        } else {
            current.push_str(line);
            out.push(current.trim().to_string());
            current = String::new();
        }
    }
    if !current.is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn split_into_blocks(lines: &[String]) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for line in lines {
        let upper = line.to_uppercase();
        if upper.starts_with("FROM ") {
            if !current.is_empty() {
                blocks.push(current);
            }
            current = vec![line.clone()];
        } else {
            current.push(line.clone());
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn parse_block(lines: Vec<String>) -> Result<BuildStage, String> {
    let from_line = &lines[0];
    let upper = from_line.to_uppercase();
    if !upper.starts_with("FROM ") {
        return Err(format!("block must start with FROM, got: {from_line}"));
    }
    let rest = &from_line[5..].trim();
    let (from, name, platform) = parse_from(rest);

    let mut instructions = Vec::new();
    for line in &lines[1..] {
        instructions.push(parse_instruction(line)?);
    }

    Ok(BuildStage {
        from,
        instructions,
        name,
        platform,
    })
}

fn parse_from(s: &str) -> (String, Option<String>, Option<String>) {
    let mut name = None;
    let mut platform = None;
    let mut rest = s.trim();

    if let Some(p) = rest.strip_prefix("--platform=") {
        let end = p.find(' ').unwrap_or(p.len());
        platform = Some(p[..end].to_string());
        rest = p[end..].trim();
    }

    let parts: Vec<&str> = rest.split_whitespace().collect();
    let image = parts[0].to_string();
    if parts.len() >= 3 && parts[1].to_uppercase() == "AS" {
        name = Some(parts[2..].join(" "));
    }

    (image, name, platform)
}

fn parse_instruction(line: &str) -> Result<Instruction, String> {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return Err(format!("bare JSON array without instruction: {trimmed}"));
    }

    let idx = line.find(char::is_whitespace).unwrap_or(line.len());
    let instruction = line[..idx].to_uppercase();
    let args = line[idx..].trim();

    match instruction.as_str() {
        "RUN" => {
            if args.starts_with('[') && args.ends_with(']') {
                let parsed: Vec<String> = serde_json::from_str(args)
                    .map_err(|e| format!("invalid RUN exec form: {e}"))?;
                Ok(Instruction::Run { command: parsed })
            } else {
                Ok(Instruction::Run {
                    command: vec!["/bin/sh".to_string(), "-c".to_string(), args.to_string()],
                })
            }
        }
        "COPY" => parse_copy_add(args, false),
        "ADD" => parse_copy_add(args, true),
        "WORKDIR" => Ok(Instruction::WorkDir(args.to_string())),
        "ENV" => {
            let (key, value) = parse_key_value(args)?;
            Ok(Instruction::Env { key, value })
        }
        "CMD" => {
            let cmd = parse_json_or_shell(args);
            Ok(Instruction::Cmd(cmd))
        }
        "ENTRYPOINT" => {
            let cmd = parse_json_or_shell(args);
            Ok(Instruction::Entrypoint(cmd))
        }
        "EXPOSE" => {
            let port: u16 = args
                .split('/')
                .next()
                .unwrap_or(args)
                .trim()
                .parse()
                .map_err(|e| format!("invalid port '{args}': {e}"))?;
            Ok(Instruction::Expose(port))
        }
        "LABEL" => {
            let (key, value) = parse_key_value(args)?;
            Ok(Instruction::Label { key, value })
        }
        "USER" => Ok(Instruction::User(args.to_string())),
        "ARG" => {
            let parts: Vec<&str> = args.splitn(2, '=').collect();
            let name = parts[0].trim().to_string();
            let default = parts.get(1).map(|s| s.trim().to_string());
            Ok(Instruction::Arg { name, default })
        }
        "SHELL" => {
            if args.starts_with('[') && args.ends_with(']') {
                let parsed: Vec<String> = serde_json::from_str(args)
                    .map_err(|e| format!("invalid SHELL exec form: {e}"))?;
                Ok(Instruction::Shell(parsed))
            } else {
                Ok(Instruction::Shell(vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                ]))
            }
        }
        "MAINTAINER" | "STOPSIGNAL" | "HEALTHCHECK" | "ONBUILD" | "VOLUME" => {
            Ok(Instruction::Comment(format!("{instruction} {args}")))
        }
        _ => Ok(Instruction::Comment(format!("{instruction} {args}"))),
    }
}

fn parse_copy_add(args: &str, _is_add: bool) -> Result<Instruction, String> {
    let mut rest = args.trim();
    loop {
        let t = rest.trim_start();
        if t.starts_with("--") {
            let end = t.find(' ').unwrap_or(t.len());
            rest = t[end..].trim();
        } else {
            break;
        }
    }

    if rest.starts_with('[') && rest.ends_with(']') {
        let parsed: Vec<String> =
            serde_json::from_str(rest).map_err(|e| format!("invalid COPY exec form: {e}"))?;
        if parsed.len() < 2 {
            return Err(format!(
                "COPY requires at least source and dest, got {parsed:?}"
            ));
        }
        let dest = parsed.last().unwrap().clone();
        let sources = parsed[..parsed.len() - 1].to_vec();
        return Ok(Instruction::Copy { sources, dest });
    }

    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!(
            "COPY requires at least source and dest, got {rest}"
        ));
    }
    let dest = parts.last().unwrap().to_string();
    let sources: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();
    Ok(Instruction::Copy { sources, dest })
}

fn parse_json_or_shell(args: &str) -> Vec<String> {
    let t = args.trim();
    if t.starts_with('[') && t.ends_with(']') {
        serde_json::from_str(t)
            .unwrap_or_else(|_| vec!["/bin/sh".to_string(), "-c".to_string(), t.to_string()])
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), t.to_string()]
    }
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let s = s.trim();
    if let Some(eq) = s.find('=') {
        let key = s[..eq].trim().to_string();
        let value = s[eq + 1..].trim().to_string();
        Ok((key, value))
    } else if let Some(space) = s.find(char::is_whitespace) {
        let key = s[..space].trim().to_string();
        let value = s[space + 1..].trim().to_string();
        Ok((key, value))
    } else {
        Ok((s.to_string(), String::new()))
    }
}

// ---------------------------------------------------------------------------
// Directory → DAG walker
// ---------------------------------------------------------------------------

/// Result of scanning a directory into DAG nodes.
/// The contained digest is a Manifest node pointing at the root layer.
pub struct DagDirectory {
    pub manifest_digest: Digest,
    pub node_count: usize,
    pub blob_bytes: u64,
}

/// Walk a directory recursively, storing every file as a DAG blob, building
/// tree/layer/manifest nodes. Returns the manifest digest.
///
/// `architecture` and `os` are recorded in the manifest node metadata.
/// Defaults to `"amd64"` and `"linux"` respectively.
pub async fn build_dag_from_directory(
    store: &Arc<MmapStore>,
    dir: &Path,
) -> Result<DagDirectory, OciError> {
    build_dag_from_directory_with_platform(store, dir, "amd64", "linux").await
}

/// Like `build_dag_from_directory` but allows specifying the target
/// platform (architecture and OS) for the generated manifest.
pub async fn build_dag_from_directory_with_platform(
    store: &Arc<MmapStore>,
    dir: &Path,
    architecture: &str,
    os: &str,
) -> Result<DagDirectory, OciError> {
    let dir = dir
        .canonicalize()
        .map_err(|e| OciError::Other(format!("canonicalize {dir:?}: {e}")))?;

    // Phase 1: walk the directory tree efficiently with walkdir,
    // collect all entries with their metadata (fast, no I/O on file content).
    let mut entries: Vec<(
        String,
        /* is_dir */ bool,
        /* is_symlink */ bool,
        u32,
    )> = Vec::new();
    for entry in WalkDir::new(&dir).sort_by_file_name() {
        let entry = entry.map_err(|e| OciError::Other(format!("walkdir: {e}")))?;
        let path = entry.path();
        let ft = entry.file_type();
        let rel = path
            .strip_prefix(&dir)
            .map_err(|_| OciError::Other("path outside root".into()))?
            .to_string_lossy()
            .to_string();
        let mode = if ft.is_dir() {
            0o755
        } else if entry
            .metadata()
            .map(|m| m.permissions().readonly())
            .unwrap_or(false)
        {
            0o444
        } else {
            0o644
        };
        entries.push((rel, ft.is_dir(), ft.is_symlink(), mode));
    }

    // Phase 2: process all files in parallel using rayon.
    // Returns HashMap<relative_path, (digest, size)> for files.
    let results: Vec<(&str, bool, bool, u32)> = entries
        .iter()
        .map(|(r, d, s, m)| (r.as_str(), *d, *s, *m))
        .collect();
    let file_results: Arc<Mutex<HashMap<String, (Digest, u64)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let symlink_results: Arc<Mutex<HashMap<String, (Digest, u64)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let store_ref: &MmapStore = store;
    results
        .par_iter()
        .for_each(|(rel, is_dir, is_symlink, _mode)| {
            if *is_dir {
                return;
            }
            let full_path = dir.join(rel);
            if *is_symlink {
                let target = match std::fs::read_link(&full_path) {
                    Ok(t) => t.to_string_lossy().to_string(),
                    Err(_) => return,
                };
                let node = DagNode::blob(target.as_bytes().to_vec());
                if let Ok(d) = store_ref.put_blocking(&node) {
                    let mut map = symlink_results.lock().expect("results lock poisoned");
                    map.insert(rel.to_string(), (d, target.len() as u64));
                }
                return;
            }
            let data = match std::fs::read(&full_path) {
                Ok(d) => d,
                Err(_) => return,
            };
            let size = data.len() as u64;
            if data.is_empty() {
                return;
            }
            let node = DagNode::blob(data.clone());
            if let Ok(d) = store_ref.put_blocking(&node) {
                if size > SMALL_FILE_THRESHOLD {
                    let _ = store_ref.put_blob_blocking(&d, &data);
                }
                let mut map = file_results.lock().expect("results lock poisoned");
                map.insert(rel.to_string(), (d, size));
            }
        });

    // Phase 3: build DirEntry tree from the collected results.
    let root_entry = build_dir_entry_tree(
        &dir,
        &dir,
        &file_results.lock().expect("results lock poisoned"),
        &symlink_results.lock().expect("results lock poisoned"),
    )?;

    // build_tree is recursive and synchronous; run in spawn_blocking.
    let store_clone = store.clone();
    let (root_layer_digest, cn, cb) = tokio::task::spawn_blocking(move || {
        let mut node_count = 0usize;
        let mut blob_bytes = 0u64;
        let d = build_tree(
            &store_clone,
            &root_entry,
            "",
            &mut node_count,
            &mut blob_bytes,
        )?;
        Ok::<_, OciError>((d, node_count, blob_bytes))
    })
    .await
    .map_err(|e| OciError::Other(format!("spawn build_tree: {e}")))??;

    let mut node_count = cn;
    let blob_bytes = cb;

    // `build_tree` already returned the root Layer node, whose prefix is
    // empty (see the call above). Wrapping it again would add a redundant
    // layer; more importantly, the prefix must stay empty here because
    // `materialize_layer` joins it onto the bundle rootfs and `Path::join`
    // discards the base when the argument is absolute ("/"), extracting
    // the rootfs into the host root instead.
    let layer_digest = root_layer_digest;

    let manifest_data = crate::ManifestData {
        entrypoint: vec![],
        cmd: vec![],
        env: vec![],
        working_dir: None,
        architecture: architecture.to_string(),
        os: os.to_string(),
        annotations: None,
        subject: None,
        user: None,
        stop_signal: None,
        exposed_ports: None,
        volumes: None,
        variant: None,
    };
    let inline = serde_json::to_vec(&manifest_data).unwrap_or_default();
    let manifest_digest = store
        .put(&DagNode::manifest(vec![layer_digest], inline))
        .await?;
    node_count += 1;

    Ok(DagDirectory {
        manifest_digest,
        node_count,
        blob_bytes,
    })
}

struct DirEntry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    symlink_target: Option<String>,
    file_digest: Digest,
    mode: u32,
    size: u64,
    uid: u32,
    gid: u32,
    mtime: i64,
    children: Vec<DirEntry>,
}

/// Build a DirEntry tree for the given directory using pre-computed file results.
/// This is synchronous and fast — no I/O on file contents (already digested).
fn build_dir_entry_tree(
    root: &Path,
    current: &Path,
    file_results: &HashMap<String, (Digest, u64)>,
    symlink_results: &HashMap<String, (Digest, u64)>,
) -> Result<DirEntry, OciError> {
    let name = if current == root {
        "/".to_string()
    } else {
        current
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // `symlink_metadata` must be used here: `metadata()` follows symlinks,
    // so a symlink would report the target's type and `is_symlink` would
    // never be true.
    let meta = std::fs::symlink_metadata(current).ok();
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let is_symlink = meta
        .as_ref()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    let mut children = Vec::new();

    // Only descend into real directories. Reading a symlink-to-directory
    // would follow the link and attribute the target's children to the
    // symlink entry itself.
    if !is_symlink {
        if let Ok(read_dir) = std::fs::read_dir(current) {
            for child in read_dir.flatten() {
                let path = child.path();
                let ft = match child.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ft.is_symlink() || ft.is_file() || ft.is_dir() {
                    let child_entry =
                        build_dir_entry_tree(root, &path, file_results, symlink_results)?;
                    children.push(child_entry);
                }
            }
        }
    }

    // Preserve the real permission bits. The previous heuristic
    // (readonly ? 0o444 : is_dir ? 0o755 : 0o644) dropped the executable
    // bit entirely, so binaries in built/committed images were not
    // executable and workloads failed to start.
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.as_ref()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(if is_dir { 0o755 } else { 0o644 })
    };
    #[cfg(not(unix))]
    let mode = meta
        .as_ref()
        .map(|m| {
            if m.permissions().readonly() {
                0o444
            } else if is_dir {
                0o755
            } else {
                0o644
            }
        })
        .unwrap_or(0o644);

    // Ownership and mtime must be carried into the DAG: the reader in
    // `converter::DirectoryEntry::from_inline_bytes` requires them, and
    // omitting them made every entry fail to parse and be dropped.
    #[cfg(unix)]
    let (uid, gid, mtime) = {
        use std::os::unix::fs::MetadataExt;
        match meta.as_ref() {
            Some(m) => (m.uid(), m.gid(), m.mtime()),
            None => (0, 0, 0),
        }
    };
    #[cfg(not(unix))]
    let (uid, gid, mtime) = (0u32, 0u32, 0i64);

    let symlink_target = if is_symlink {
        std::fs::read_link(current)
            .ok()
            .map(|t| t.to_string_lossy().to_string())
    } else {
        None
    };

    let (file_digest, size) = if is_dir {
        (Digest([0u8; 32]), 0)
    } else if is_symlink {
        let rel = path_to_rel(root, current);
        if let Some((d, sz)) = symlink_results.get(&rel) {
            (*d, *sz)
        } else {
            (Digest([0u8; 32]), 0)
        }
    } else {
        let rel = path_to_rel(root, current);
        if let Some((d, sz)) = file_results.get(&rel) {
            (*d, *sz)
        } else {
            (Digest([0u8; 32]), 0)
        }
    };

    Ok(DirEntry {
        name,
        is_dir,
        is_symlink,
        symlink_target,
        file_digest,
        mode,
        size,
        uid,
        gid,
        mtime,
        children,
    })
}

fn path_to_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Recursively build DAG tree nodes from a DirEntry tree (synchronous).
///
/// Mirrors `OciToDagConverter::store_trees`: every directory (including the
/// root, whose `base_path` is empty) is encoded as a `Layer` node wrapping
/// its `Tree` node, with the directory's relative path in `inline_data`.
/// The materializer relies on that path to place nested entries, and on it
/// being *empty* for the root so `rootfs.join(prefix)` does not escape.
fn build_tree(
    store: &MmapStore,
    entry: &DirEntry,
    base_path: &str,
    node_count: &mut usize,
    blob_bytes: &mut u64,
) -> Result<Digest, OciError> {
    if !entry.is_dir {
        if entry.file_digest.0 != [0u8; 32] {
            *blob_bytes += entry.size;
        }
        return Ok(entry.file_digest);
    }

    let mut child_digests = Vec::new();
    let mut child_entry_data = Vec::new();

    for child in &entry.children {
        let child_path = if base_path.is_empty() {
            child.name.clone()
        } else {
            format!("{}/{}", base_path, child.name)
        };
        let d = build_tree(store, child, &child_path, node_count, blob_bytes)?;
        child_digests.push(d);

        // Serialize with the *reader's* own encoder so the on-disk
        // schema can never drift from `DirectoryEntry::from_inline_bytes`.
        // Hand-rolling this JSON previously omitted uid/gid/mtime/
        // symlink_target, so every entry failed to parse and was
        // silently dropped at materialization time (empty rootfs).
        let dir_entry = crate::DirectoryEntry {
            name: child.name.clone(),
            digest: d,
            mode: child.mode,
            size: child.size,
            uid: child.uid,
            gid: child.gid,
            mtime: child.mtime,
            is_dir: child.is_dir,
            is_symlink: child.is_symlink,
            symlink_target: child.symlink_target.clone(),
            xattrs: HashMap::new(),
        };
        child_entry_data.push(dir_entry.to_inline_bytes());
    }

    let inline: Vec<u8> = child_entry_data.concat();

    let tree_node = DagNode::tree(child_digests, inline);
    let tree_digest = store
        .put_blocking(&tree_node)
        .map_err(|e| OciError::Other(format!("store tree: {e}")))?;
    *node_count += 1;

    let layer_node = DagNode::layer(vec![tree_digest], base_path.as_bytes().to_vec());
    let layer_digest = store
        .put_blocking(&layer_node)
        .map_err(|e| OciError::Other(format!("store layer: {e}")))?;
    *node_count += 1;

    Ok(layer_digest)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_dockerfile() {
        let df = Dockerfile::parse("FROM alpine:3.18\nRUN echo hello\nCMD [\"echo\", \"hi\"]\n")
            .unwrap();
        assert_eq!(df.stages.len(), 1);
        assert_eq!(df.stages[0].from, "alpine:3.18");
        assert_eq!(df.stages[0].instructions.len(), 2);
    }

    #[test]
    fn test_parse_multi_stage() {
        let df = Dockerfile::parse(
            "FROM golang:1.21 AS builder\nRUN go build -o /app .\nFROM alpine:3.18\nCOPY --from=builder /app /app\n",
        )
        .unwrap();
        assert_eq!(df.stages.len(), 2);
        assert_eq!(df.stages[0].name.as_deref(), Some("builder"));
        assert_eq!(df.stages[1].from, "alpine:3.18");
    }

    #[test]
    fn test_parse_run_shell_form() {
        let df = Dockerfile::parse("FROM ubuntu\nRUN apt-get update && apt-get install -y curl\n")
            .unwrap();
        match &df.stages[0].instructions[0] {
            Instruction::Run { command } => {
                assert_eq!(command[0], "/bin/sh");
                assert_eq!(command[1], "-c");
                assert!(command[2].contains("apt-get update"));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_run_exec_form() {
        let df =
            Dockerfile::parse("FROM ubuntu\nRUN [\"apt-get\", \"install\", \"-y\", \"curl\"]\n")
                .unwrap();
        match &df.stages[0].instructions[0] {
            Instruction::Run { command } => {
                assert_eq!(command, &["apt-get", "install", "-y", "curl"]);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_copy() {
        let df = Dockerfile::parse("FROM alpine\nCOPY package.json /app/\nCOPY src/ /app/src/\n")
            .unwrap();
        match &df.stages[0].instructions[0] {
            Instruction::Copy { sources, dest } => {
                assert_eq!(sources, &["package.json"]);
                assert_eq!(dest, "/app/");
            }
            _ => panic!("expected Copy"),
        }
    }

    #[test]
    fn test_parse_env_workdir() {
        let df = Dockerfile::parse("FROM alpine\nENV NODE_ENV=production\nWORKDIR /app\n").unwrap();
        let env = &df.stages[0].instructions[0];
        match env {
            Instruction::Env { key, value } => {
                assert_eq!(key, "NODE_ENV");
                assert_eq!(value, "production");
            }
            _ => panic!("expected Env"),
        }
        let wd = &df.stages[0].instructions[1];
        match wd {
            Instruction::WorkDir(path) => assert_eq!(path, "/app"),
            _ => panic!("expected WorkDir"),
        }
    }

    #[test]
    fn test_parse_expose() {
        let df = Dockerfile::parse("FROM nginx\nEXPOSE 80\nEXPOSE 443/tcp\n").unwrap();
        match &df.stages[0].instructions[0] {
            Instruction::Expose(port) => assert_eq!(*port, 80),
            _ => panic!("expected Expose"),
        }
        match &df.stages[0].instructions[1] {
            Instruction::Expose(port) => assert_eq!(*port, 443),
            _ => panic!("expected Expose"),
        }
    }

    #[test]
    fn test_parse_comments_and_continuation() {
        let df =
            Dockerfile::parse("# This is a comment\nFROM alpine\nRUN echo hello \\\n    world\n")
                .unwrap();
        assert_eq!(df.stages.len(), 1);
        match &df.stages[0].instructions[0] {
            Instruction::Run { command } => {
                assert!(command[2].contains("hello"), "got: {:?}", command[2]);
                assert!(command[2].contains("world"), "got: {:?}", command[2]);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_from_with_platform() {
        let df =
            Dockerfile::parse("FROM --platform=linux/amd64 alpine:3.18 AS base\nRUN echo hi\n")
                .unwrap();
        assert_eq!(df.stages.len(), 1);
        assert_eq!(df.stages[0].platform.as_deref(), Some("linux/amd64"));
        assert_eq!(df.stages[0].name.as_deref(), Some("base"));
    }

    /// Regression test for a host-rootfs escape bug.
    ///
    /// `build_dag_from_directory_with_platform` used to give the root layer
    /// node a `"/"` prefix. `OciMaterializer::materialize_layer` joins that
    /// prefix onto the bundle rootfs path, and `Path::join` discards the base
    /// when the argument is absolute:
    ///
    ///     Path::new("/var/lib/pullrun/bundles/abc/rootfs").join("/") == "/"
    ///
    /// so materializing a committed or built image extracted its contents
    /// into the host root instead of the bundle. Because it also silently
    /// discarded the prefix, this only manifested at materialize time — well
    /// after `commit`/`build` returned success.
    ///
    /// The assertion is deliberately on the *stored node prefix*, before any
    /// materialization: asserting here means a regression fails the test
    /// instead of writing to the developer's `/`.
    #[tokio::test]
    async fn test_build_from_directory_root_layer_has_empty_prefix() {
        let src = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("etc")).unwrap();
        std::fs::write(src.path().join("etc/passwd"), b"root:x:0:0\n").unwrap();
        std::fs::write(src.path().join("hello.txt"), b"hello").unwrap();

        let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));
        let built = build_dag_from_directory_with_platform(&store, src.path(), "arm64", "linux")
            .await
            .expect("build from directory");

        // manifest -> root layer
        let manifest = store.get_archived(&built.manifest_digest).unwrap();
        assert!(manifest.is_manifest(), "root node must be a manifest");
        assert_eq!(
            manifest.edges.len(),
            1,
            "build_from_directory must produce exactly one layer"
        );
        let layer_digest = Digest(manifest.edges[0]);
        drop(manifest);

        let layer = store.get_archived(&layer_digest).unwrap();
        assert!(layer.is_layer(), "child node must be a layer");

        assert!(
            layer.inline_data.is_empty(),
            "root layer prefix must be empty, got {:?} — a non-empty \
             absolute prefix makes materialization escape the rootfs \
             (see materialize_layer)",
            String::from_utf8_lossy(&layer.inline_data)
        );
    }

    /// Companion to the test above: the DAG built from a directory must
    /// materialize *inside* the requested directory, with content intact.
    ///
    /// This covers three defects that previously made `build`/`commit`
    /// produce an unusable image:
    ///  1. entries were serialized with a schema the reader could not
    ///     parse, so they were silently dropped (empty rootfs);
    ///  2. subdirectories were not wrapped in path-carrying Layer nodes,
    ///     so nested files flattened to the root;
    ///  3. symlinks were written with `is_symlink: false`, so they
    ///     materialized as regular files containing the target path.
    #[tokio::test]
    async fn test_build_from_directory_materializes_inside_target() {
        let src = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("etc")).unwrap();
        std::fs::create_dir_all(src.path().join("bin")).unwrap();
        std::fs::write(src.path().join("etc/passwd"), b"root:x:0:0\n").unwrap();
        std::fs::write(src.path().join("hello.txt"), b"hello world").unwrap();
        std::fs::write(src.path().join("bin/run"), b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                src.path().join("bin/run"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink("hello.txt", src.path().join("link.txt")).unwrap();
        }

        let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));
        let built = build_dag_from_directory_with_platform(&store, src.path(), "arm64", "linux")
            .await
            .expect("build from directory");

        // Never materialize a DAG whose prefix would escape the target.
        let manifest = store.get_archived(&built.manifest_digest).unwrap();
        let layer_digest = Digest(manifest.edges[0]);
        drop(manifest);
        let layer = store.get_archived(&layer_digest).unwrap();
        let prefix = String::from_utf8_lossy(&layer.inline_data).to_string();
        drop(layer);
        assert!(
            !prefix.starts_with('/'),
            "refusing to materialize: non-relative layer prefix {prefix:?}"
        );

        let materializer = crate::OciMaterializer::new(&store);
        let bundle = materializer
            .materialize_bundle(&built.manifest_digest, out_dir.path())
            .expect("materialize bundle");

        let rootfs = bundle.rootfs_path;
        assert_eq!(
            std::fs::read(rootfs.join("hello.txt")).unwrap(),
            b"hello world",
            "file must land inside the bundle rootfs"
        );
        assert_eq!(
            std::fs::read(rootfs.join("etc/passwd")).unwrap(),
            b"root:x:0:0\n",
            "nested file must land inside the bundle rootfs"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::symlink_metadata(rootfs.join("link.txt")).unwrap();
            assert!(
                meta.file_type().is_symlink(),
                "symlink must materialize as a symlink, not a regular file"
            );
            assert_eq!(
                std::fs::read_link(rootfs.join("link.txt")).unwrap(),
                std::path::Path::new("hello.txt")
            );

            let mode = std::fs::metadata(rootfs.join("bin/run"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "executable bit must be preserved, got {mode:o}"
            );
        }
    }
}
