# Changelog

## 0.10.2 — 2026-11-23

- CRI mount propagation fixed via open_tree/move_mount: rshared/rslave
  now propagate host↔container. Mount suite 4/4.
- Container full suite 30/31 (only feature-gated image volumes remain).
- CRI conformance 50/50 unchanged.

## 0.10.1 — 2026-11-23

- CRI conformance subset **50/50** on Kubernetes 1.37.
- Pod hostname is applied to the sandbox and joined containers.
- CRI `PortMappings` are translated into host-port forwarding rules.
- Networking suite 4/4 (hostname + host/container port mapping).

## 0.10.0 — 2026-11-23

Kubernetes 1.37 CRI support, MCP 2026-07-28 stateless HTTP, and the
stateful-agent layer (snapshots, actors, credential injection).

### CRI (Kubernetes 1.37, critest v1.37)
- `k8s.io/cri-api v0.37.2`; Runtime info 2/2, PodSandbox 9/9,
  Container basics 14/14, logs 2/2, stats 5/5, Image 15/15,
  Streaming 5/5.
- Full Container suite 28/31; remaining `rshared`/`rslave` mount
  propagation and the feature-gated image volume.
- Container Created/Start semantics, CRI log format and reopen,
  netns-joined pods, hostNetwork, port-forward, TTY exec, image
  digest/RepoDigests/Uid/Username, idempotent stops.
- OCI DAG conversion batches writes and fsyncs once (httpd converts in
  ~25 s).

### MCP
- mcp-go v1.6.0, protocol 2026-07-28; Streamable HTTP (`--http`) and
  stateless mode (`--stateless`), discovery, per-request headers.

### Stateful agents
- Firecracker MEMORY/ROOTFS snapshots, templates/actors with resume,
  measured restore latency.
- Fail-closed egress and HTTPS credential injection with no sandbox
  secrets.
- Security status matrix and metrics.

### Docs
- `docs/CRI.md`, `docs/ALL_MCP.md`, `docs/SECURITY_STATUS.md`,
  `docs/SNAPSHOTS.md`, `docs/ACTORS.md`, `docs/EGRESS.md`.

## Unreleased

### CRI
- Container labels/annotations/attempt preserved; ExecSync now
  propagates the timeout to the runtime, detaches the exec from client
  cancellation, and kills the timed-out process via the runc pid-file.
  Container basic operations: 14/14.
- Host networking no longer creates an OCI network namespace.
- Per-pod bridge falls back to the shared bridge when no /24 subnet can
  be allocated, avoiding a gateway route conflict.
- OCI DAG conversion batches node writes and fsyncs once; after a clean
  rebuild the httpd image converts in ~25 s (previously >10 minutes).
- StopContainer is idempotent; OOM exits report `OOMKilled`; CRI
  mount propagation maps to rprivate/rslave/rshared and ContainerStatus
  returns mounts. Privileged capabilities use CAP_* names (rprivate and
  readonly mount specs pass; rshared/rslave still fail).
- Full Container suite: 28/31 expected (multi-container 3/3).
- Image core: 15/15 (digest RepoDigests, reference status, Uid/Username
  from image config).
- Streaming: 5/5. Port-forward uses the portforward.k8s.io handshake
  and a non-blocking callback; TTY exec buffers exit status until the
  error stream opens and drains resize streams; v5 is advertised with
  v4 fallback.
- Container basic operations: 13/14 (timed-out exec process cleanup
  still fails).
- Container stats now honor the CRI label selector (`5/5` stats specs).
- Exec: `NetworkMode::Host` now omits the OCI network namespace, so
  hostNetwork pods actually share the host netns.
- Streaming: TTY is forwarded into AttachOpen and hostNetwork
  port-forward targets 127.0.0.1; suite re-verification pending.
- Image refs are indexed per tag (ref → root) instead of root → last
  ref, so multiple tags of one image remain addressable.
- RemoveImage resolves tags through HasImage, removes all refs for the
  root, and is idempotent when the image is absent.
- ImageStatus/ListImages expose Uid/Username and group RepoTags by
  image ID; ContainerStatus reports ImageId.
- OCI puller: split `repo@sha256:...` before the tag colon; digest
  pulls no longer build `repo@sha256` as the repository name.
- ImageStatus canonicalizes untagged refs to `:latest` and reports no
  RepoTags for digest references.
- The puller computes and persists the registry manifest digest;
  ListImages only exposes tagged refs (digest aliases are internal).
- OCI puller: use `/v2/token` for non-Docker registries. gcr.io
  redirects `/token` to an HTML page, which broke all gcr.io pulls.
- Upgrade to Kubernetes 1.37 CRI API (`k8s.io/cri-api v0.37.2`).
- Implement `HasImage` with image size; `ImageStatus` works.
- Preserve PodSandbox metadata attempt, labels, annotations.
- Propagate Linux pod sysctls to the sandbox and every container.
- Idempotent StopPodSandbox/StopContainer for missing workloads.
- CRI Created state: create-only workloads plus a StartWorkload RPC;
  StartContainer no longer races a command that already ran.
- Container logs are written to the CRI LogPath in CRI format
  (RFC3339Nano/stream/tag); relative paths resolve against the pod log
  directory; ReopenContainerLog is followed by the writer. Log
  conformance: 2/2.
- critest v1.37: Runtime info 2/2, PodSandbox 9/9. Container suite
  rerun in progress; image/streaming/stats not yet run.

## Unreleased

### MCP
- Upgrade mcp-go to v1.6.0 (protocol 2026-07-28, 2025-11-25 and older).
- Add Streamable HTTP transport: `pullrun mcp --http ADDR`, endpoint
  `/mcp`, with `--stateless` for session-less deployments.
- Legacy `--sse` remains available. Verified 2026-07-28
  `server/discover` and `tools/list` over stateless HTTP (15 tools, no
  `Mcp-Session-Id`).

## Unreleased

### Credential injection
- `--egress-credential HOST:HEADER=SECRET` starts a per-daemon HTTPS
  MITM proxy with a generated CA. Workloads get proxy env and a
  read-only CA mount; the secret is injected by the proxy and never
  placed in the sandbox.
- Verified with `curlimages/curl`: response contained
  `Authorization: Bearer test123`, with no `/run/secrets` mount and no
  secret env var.
- Proxy accepts connections only from loopback/private source
  addresses.

## 0.9.1 — 2026-11-22

Rootful container networking and verified fail-closed egress.

### Networking
- Bring the `pullrun-br0` bridge up even when it already exists, add
  MASQUERADE for the default `10.42.0.0/16`, and add FORWARD accept
  rules (Docker sets FORWARD to DROP). Rootful containers now have
  working egress.
- Fail-closed egress is verified end to end: with
  `--egress-deny --allow-outbound tcp:1.1.1.1:80`, a TCP connect to
  `1.1.1.1:80` exits 0 and a connect to `93.184.216.34:80` exits 1.
- `docs/EGRESS.md` and `docs/SECURITY_STATUS.md` updated: egress is
  **Met** for rootful containers; VM/rootless still reject the flag.

## 0.9.0 — 2026-11-22

Stateful-agent layer: Firecracker memory snapshots, golden-template
actors with suspend/resume, fail-closed egress (opt-in), and
snapshot/actor metrics.

### Snapshots
- `pullrun snapshot create|ls|rm|restore` for Firecracker VMs.
- MEMORY fidelity: guest RAM + device state + rootfs; source VM stops.
- ROOTFS fidelity: disk-only, source VM keeps running, restore cold-boots.
- Restore latency: 63 / 84 / 119 ms (best/avg/worst) after sparse
  rootfs copies; capture ~3.8 s for a 512 MiB guest.
- Snapshot metadata records fidelity, kernel, vcpus, memory, drive path.

### Actors and templates
- `pullrun template create --from-workload`, `actor create|ls|resume|suspend|run|rm`.
- Golden template = memory snapshot; actors start suspended.
- Resume restores memory in 99–110 ms; suspend writes a new snapshot.
- Per-actor mutex serializes transitions (request parking).
- Restored VMs track their actual drive path (fixes suspend after resume).

### Fail-closed egress (opt-in)
- `--egress-deny` per workload or daemon; rootful containers get an
  nftables drop-by-default output chain with DNS + `--allow-outbound`
  allowlist. VM/rootless reject the flag.
- Mechanism verified in a netns; full traffic path still pending due a
  rootful container veth/network issue on the test host.

### Observability and docs
- Snapshot capture/restore and actor resume/suspend Prometheus metrics.
- `docs/SNAPSHOTS.md`, `docs/ACTORS.md`, `docs/EGRESS.md`, and
  `docs/SECURITY_STATUS.md` (honest Met/Partial/Future matrix).

### Known limitations
- Credential injection / zero ambient credentials: not implemented.
- VOLUMES fidelity: unsupported.
- Egress end-to-end traffic: unverified; VM egress denied is rejected.
- Snapshot files unencrypted; no automatic GC/retention.
- Actors single-node; no HTTP ingress queue.

## 0.8.2 — 2026-11-22

Firecracker VM backend repair. The 0.8.0 Linux VM path never started:
Firecracker rejected the config flag, then exited on metrics
initialization. This release makes `run --backend vm` boot, execute the
requested command, power off, and return the guest exit code.

### Firecracker
- Use Firecracker's `--config-file` (not `--config-path`).
- Drop `--metrics-path`; Firecracker expects an existing FIFO and
  otherwise exits with `MetricsInitialization(ENOENT)`.
- Isolated/`--net none` VMs no longer create a tap, bridge, or virtio
  NIC; the sidecar records `network_disabled` and start/stop skip
  network plumbing.
- Execute the workload command from `/init` instead of always running
  the OCI ENTRYPOINT/CMD; print `PULLRUN_EXIT=<code>` on ttyS0 and
  power off (`/proc` is mounted so sysrq works).
- `status()` treats the marker as the VM exit and reaps the Firecracker
  process; the CLI polls workload state while attached and returns the
  exit code.
- The boot smoke test builds its ext4 rootfs with `mkfs.ext4 -d` (no
  loop mount), so it runs unprivileged with `/dev/kvm`.

### Runtime / CLI
- `GetWorkload` returns live container exit codes instead of waiting
  up to 5 s for the background watcher (including `container-rootless`).
- Attach drains queued output briefly after `exited`.

### Known limitations
- Firecracker tier timing is still pending (`docs/TIERS.md`).
- Linux sandbox groups are not implemented.

## 0.8.0 — 2026-11-21

Security and correctness release. Fixes host-filesystem escapes in OCI
extraction, restores real exit codes for VMs and containers, honours the
workload spec in rootless mode, and replaces per-workload image
extraction with immutable image bases plus copy-on-write clones. Adds
sandbox groups (one Apple VM, concurrent sandboxes), container
overlayfs mode, and pull retries.

**Upgrade notes:** macOS VM rootfs now lives under
`<store>/rootfs/<workload-id>/` (per-workload clone) instead of a shared
per-digest directory; images built or committed with ≤0.7.9 still need
rebuilding. Rootless containers now run as container uid 0 mapped to the
host user. `commit` captures filesystem state only — not guest memory.

### Security
- **Confined OCI extraction.** Layer tars are now resolved with a
  descriptor-relative `RootDir`: absolute paths and parent components
  are rejected, symlink targets are re-rooted inside the rootfs, and
  final operations use `*at` syscalls. Fixes symlink+whiteout escapes
  and absolute whiteout targets from crafted DAG imports.
- **Opaque whiteouts fixed.** The converter previously dropped every
  `.wh..wh..opq` because it filtered on `contains("..")`; path
  validation is now component-based and root opacity clears only the
  bundle.

### Correctness
- **Apple VM fast exits.** `ExitAck` handshake, no `SHUT_WR` on the
  duplex vsock, and guest stdout/stderr drained to EOF. Fast commands
  keep their output and exit code; the daemon records the real code
  instead of defaulting to 137.
- **Container exit codes.** Containers run under a foreground `runc`
  child, so `exited(<code>)` is recorded and returned; startup errors
  are distinguished from workload exits via captured stderr.
- **Rootless honours the workload spec.** Command, env, mounts,
  working dir, security flags, and limits are built by a shared OCI
  config builder; the process runs as container uid 0 mapped to the
  host user instead of an unmapped host uid.
- **Stale runc state** is cleared before start.

### Storage
- **Immutable image bases** (`pullrun-oci::base`): one flattened tree
  per digest, single-flight build, atomic rename, `.ready` marker, and
  GC on `prune`/`gc`.
- **Copy-on-write clones** (`pullrun-oci::cow`): APFS `clonefile`,
  Linux `FICLONE`, copy fallback, capability probe. Per-task containers
  and macOS VMs get private clones instead of sharing one writable
  rootfs.
- **Firecracker ext4 bases** are built once per digest and cloned with
  `FICLONE` on reflink filesystems; ext4 hosts build per VM.
- **`--rootfs-overlay`** for containers: base as read-only lower layer,
  private upper/work per sandbox.

### Sandbox groups (macOS)
- `pullrun group create/run/ls/rm`: one Apple VM, concurrent sandboxes
  with mount/net/UTS/IPC namespaces and per-sandbox tmpfs overlays.
- Id-tagged vsock frames (`SandboxSpec/Stdout/Stderr/Exit/Error`);
  concurrent sandboxes share the guest page cache.
- Measured: 10 trivial sandboxes in ~185 ms; the image cache warms
  6 MiB once and adds zero per extra reader (`docs/GROUPS.md`).

### Reliability
- OCI pulls retry transient connect/timeout/5xx failures (3 attempts,
  10 s connect timeout) and use `registry-1.docker.io`.
- Linux clippy/test fixes and a CI-safe overlay userns test.

## 0.7.10 — 2026-11-07

### build/commit produced unusable images (four defects)
All four were in the `pullrun build` / `pullrun commit` path and only
manifested once the resulting image was materialized.

- **Root layer prefix escaped the rootfs** (`pullrun-oci/dockerfile.rs`).
  The root Layer node used the prefix `"/"`, and `materialize_layer` joins
  that prefix onto the bundle rootfs path. `Path::join` discards the base
  when the argument is absolute, so `rootfs.join("/")` is `/` and
  materialization extracted the image into the **host root** instead of the
  bundle. The prefix is now empty at the root, matching `OciToDagConverter`.
- **Tree-entry schema drift emptied the rootfs** (`pullrun-oci`).
  `build_tree` hand-wrote tree entries with 6 fields; the reader,
  `DirectoryEntry::from_inline_bytes`, requires 11 (uid, gid, mtime and
  symlink_target were omitted). Parsing failed for every entry and the
  `if let Ok(..)` reader dropped them silently, so built and committed
  images materialized with a completely **empty rootfs** while `commit`
  reported success. Entries now go through `DirectoryEntry::to_inline_bytes`,
  the reader's own encoder, and the reader logs instead of discarding.
- **Nested directories flattened to the image root.** Subdirectories were
  not wrapped in path-carrying Layer nodes, so `etc/passwd` landed at
  `/passwd`. `build_tree` now mirrors `OciToDagConverter::store_trees`.
- **Symlinks and file modes were lost.** `build_dir_entry_tree` used
  `metadata()`, which follows symlinks, so `is_symlink` was always false and
  links materialized as regular files holding the target path; the mode
  heuristic collapsed everything to `0644`, dropping the executable bit.
  Now uses `symlink_metadata()`, carries `symlink_target`, and preserves the
  real permission bits.

> **Upgrade note:** images built or committed with 0.7.9 or earlier carry the
> `"/"` prefix and the old 6-field schema in their DAG nodes. This fix does
> not repair stored data — **rebuild those images**.

### commit deadlocked on any tagged image
`CommitImage` held the `image_tags` write guard across `save_image_tags()`,
which takes a read guard on the same `RwLock`. `tokio::sync::RwLock` is not
reentrant, so `pullrun commit <id> <tag>` hung forever and leaked the op lock.

### Dependencies
- `rustls` 0.23.45 → 0.23.46, fixing GHSA-c8m5-c952-6cm4 (medium) plus
  GHSA-h2v8-394m-p7vv, GHSA-w44r-93j7-qv28 and GHSA-vp88-798v-h67f (low),
  across the main workspace and the apple-virt/vm-outbound tool lockfiles.

### Tests
250 tests (197 Rust + 53 Go). Adds three regression tests, each verified to
fail against the unfixed code: the root-layer prefix, end-to-end
materialization (content, nesting, symlink type/target, executable bit), and
a commit-with-tag deadlock guard that drives the real RPC under a timeout.

## 0.7.9 — 2026-07-31

- **CRI exec/attach correctness.** `serveAttach` handshakes
  `streamProtocolV4` before the SPDY upgrade; v1 never closed stdin at EOF,
  which hung `crictl exec -i` with piped stdin. Exit codes travel on the
  error stream as a hand-rolled `metav1.Status` (`StatusCause.Type`
  marshals as `reason`, not `type`).
- **Detached stop.** The stop sequence runs in a spawned task so a cancelled
  RPC (crictl's 2s default timeout) cannot abort cleanup and strand
  containers in stale `running` state.
- **Per-pod `/24` bridge subnets with NAT.** Each pod gets `pr-<podid>` and
  its own subnet; `subnet_for`/`live_subnets_in_range` skip ranges owned by
  live pods so leaked sandboxes cannot collide with new ones.
- **Non-Docker registries.** `image_parts` no longer applies the `library/`
  prefix outside docker.io, fixing 404s against registry.k8s.io, ghcr.io and
  quay.io.
- **Pod networking.** Containers join the sandbox netns through the OCI
  spec's `linux.namespaces[].path` rather than the nonexistent
  `runc --network` flag; the shim gained real pod/container modelling,
  volume mounts and pod DNS.

## 0.7.8 — 2026-07-31

### CRI
- Container labels/annotations/attempt preserved; ExecSync now
  propagates the timeout to the runtime, detaches the exec from client
  cancellation, and kills the timed-out process via the runc pid-file.
  Container basic operations: 14/14.
- Host networking no longer creates an OCI network namespace.
- Per-pod bridge falls back to the shared bridge when no /24 subnet can
  be allocated, avoiding a gateway route conflict.
- OCI DAG conversion batches node writes and fsyncs once; after a clean
  rebuild the httpd image converts in ~25 s (previously >10 minutes).
- StopContainer is idempotent; OOM exits report `OOMKilled`; CRI
  mount propagation maps to rprivate/rslave/rshared and ContainerStatus
  returns mounts. Privileged capabilities use CAP_* names (rprivate and
  readonly mount specs pass; rshared/rslave still fail).
- Full Container suite: 28/31 expected (multi-container 3/3).
- Image core: 15/15 (digest RepoDigests, reference status, Uid/Username
  from image config).
- Streaming: 5/5. Port-forward uses the portforward.k8s.io handshake
  and a non-blocking callback; TTY exec buffers exit status until the
  error stream opens and drains resize streams; v5 is advertised with
  v4 fallback.
- Container basic operations: 13/14 (timed-out exec process cleanup
  still fails).
- Container stats now honor the CRI label selector (`5/5` stats specs).
- Exec: `NetworkMode::Host` now omits the OCI network namespace, so
  hostNetwork pods actually share the host netns.
- Streaming: TTY is forwarded into AttachOpen and hostNetwork
  port-forward targets 127.0.0.1; suite re-verification pending.
- Image refs are indexed per tag (ref → root) instead of root → last
  ref, so multiple tags of one image remain addressable.
- RemoveImage resolves tags through HasImage, removes all refs for the
  root, and is idempotent when the image is absent.
- ImageStatus/ListImages expose Uid/Username and group RepoTags by
  image ID; ContainerStatus reports ImageId.
- OCI puller: split `repo@sha256:...` before the tag colon; digest
  pulls no longer build `repo@sha256` as the repository name.
- ImageStatus canonicalizes untagged refs to `:latest` and reports no
  RepoTags for digest references.
- The puller computes and persists the registry manifest digest;
  ListImages only exposes tagged refs (digest aliases are internal).
- OCI puller: use `/v2/token` for non-Docker registries. gcr.io
  redirects `/token` to an HTML page, which broke all gcr.io pulls. shim: real pod/container runtime
- **Pod network namespace sharing** (exec/container.rs, exec/rootless.rs). The
  shim now runs a pause sandbox workload (`registry.k8s.io/pause:3.9`) that
  anchors the pod's network namespace; containers run their own image and join
  it via `network_mode: container:<sandbox>`. Joining is implemented through
  the OCI spec (`linux.namespaces[].path` resolved via `runc state`), not the
  nonexistent `runc --network` flag. VM backends reject the mode (no netns
  sharing possible) and use the per-pod bridge instead. Failed `runc run -d`
  now cleans up leftover runc state so retries don't hit
  "container already exists".
- **Real container lifecycle** (cri/pullrun-cri). Containers get restart-safe
  deterministic workload ids, run their own image via `RunWorkload`, and have
  a real create/start/stop/remove flow instead of the previous 1:1 alias to
  the sandbox. Sandbox = pause workload with a per-pod bridge and pod IP.
- **Volume mounts** (cri/pullrun-cri). CRI `ContainerConfig.Mounts` are mapped
  to runtime bind mounts (rbind+rprivate, `ro` for readonly) with the
  daemon's existing `sanitize_mounts` validation.
- **Pod DNS** (proto, exec/container.rs, cri/pullrun-cri). New
  `RunRequest.dns` (nameservers/searches/options) is written into the
  workload's `/etc/resolv.conf` by the container executors; the shim takes
  `PodSandboxConfig.DnsConfig` and propagates it to every container in the
  pod. Persisted in `WorkloadState` so restarts keep it.
- **Security context mapping** (cri/pullrun-cri). `privileged`,
  `readonlyRootfs`, `noNewPrivileges`, seccomp (runtime-default/unconfined),
  and `hostNetwork` are mapped onto the workload spec.
- **Stats** (cri/pullrun-cri). `ContainerStats`, `PodSandboxStats`, and
  `ListPodSandboxStats` are backed by `GetWorkloadStats`.
- **Version 1.0.0 + startup reconciliation** (cri/pullrun-cri). The shim
  reports CRI version 1.0.0 with real runtime status probes, and on startup
  reconciles its checkpoints against the runtime's `ListWorkloads` (pods and
  containers without a live workload are marked exited; stale workloads are
  recovered as exited).
- **CRI pause image warm-up** (cri/pullrun-cri). The pause image is pulled
  into the DAG store at shim startup, so sandbox creation never blocks on a
  network fetch.

### 0.7.7 — 2026-07-31

### Critical fixes
- **Unauthenticated daemon socket** (main.rs). The gRPC socket is now chmod 0700
  and every accepted connection is verified via `SO_PEERCRED` against the
  daemon's own euid before tonic sees it — other local users can no longer
  reach the run/exec/copy surface, which was a local privilege-escalation
  primitive on multi-user hosts. Daemon logs moved to stderr so CLI/MCP
  stdout output stays clean.
- **Path traversal via workload id** (service.rs). `validate_workload_id`
  (ASCII alphanumeric + `-_.`, max 128 chars) now guards `run_workload`,
  `stop_workload`, `exec_in_workload`, and `attach_workload` before ids are
  used to build bundle paths.
- **Arbitrary bind mounts** (service.rs). `sanitize_mounts` rejects mounts
  that escape the store root, non-absolute or `..` destinations, and unknown
  mount types at the gRPC boundary.
- **Seccomp/readonly/noNewPrivileges parsed but never applied** (container.rs,
  seccomp.rs). `maskedPaths`, `readonlyPaths`, read-only `/sys`,
  `readonly_rootfs`, `noNewPrivileges`, and the seccomp policy (`default`
  allowlist, `unconfined`, `pullrun:<json>`, fail-closed on unknown profiles)
  are now emitted into the runc OCI spec and carried through restarts.
- **`exec_in_workload` executed commands twice** (service.rs). The redundant
  capture `runc exec` was removed; `Executor::exec` now returns exit code plus
  captured stdout/stderr in a single call.
- **Staged secrets world-readable** (secrets.rs, service.rs, store.rs).
  Staging dirs/files are created 0700/0600 (`create_new` + fsync), the secret
  key is fsync'd before use, and the store root defaults to 0700.

### High-priority fixes
- **Port-forward listeners leaked on container stop** (container.rs). Proxy
  sessions are now torn down before SIGTERM so inbound forwards can't keep
  forwarding to a dead workload IP.
- **Cosign signature not bound to image content** (policy/lib.rs). The signed
  payload's image ref and manifest digest must now match the image being
  evaluated — a valid signature for image X can no longer authorize image Y.
- **Firecracker tap name collisions** (pool.rs). Pool UUIDs now use RFC 4122
  v4 entropy instead of a 4.29s-granularity clock-derived value.

### Medium-priority fixes
- **`--registry` silently overrode the ref's registry** (puller.rs).
  `pullrun pull ghcr.io/user/app` no longer pulls `docker.io/user/app`; the
  flag now only applies to bare refs (e.g. `--registry localhost:5000 alpine`).

## 0.7.7 — 2026-07-31

- **Security hardening.** Daemon authentication, path-traversal fixes,
  seccomp enforcement, and restrictive permissions on secret files.
- Go 1.26 across all modules, `golangci-lint` clean, and `make fmt` covering
  every module.

## 0.7.6 — 2026-07-26

### 0.7.6 follow-up (4ec2891)
- **`streaming.go` IPv6 compatibility** — replaced `fmt.Sprintf("%s:%s")` with `net.JoinHostPort`.
- **`runtime-daemon.yaml` image tag** — bumped from `v0.3.0` to `v0.7.6`.
- **`events.go --follow` flag now functional** — exits after one event when `--follow=false`.
- **`builder.rs` `bytes_stored` always 0** — `total_bytes` now accumulated from each `execute_run`/`execute_copy` call instead of stuck at `0u64`.
- **Dead `sandboxStore` type removed** (`main.go`) — in-memory store was replaced by `fileStore` but never deleted.
- **CI matrix expanded** — Go build/test/vet/lint now covers CRI, compose, and control-plane in addition to CLI.
- **Makefile `lint-go` target added** — runs `golangci-lint` on all 4 Go modules; `build-go`/`test-go`/`fmt` also cover all modules.

### Critical fixes
- **Path traversal via COPY/ADD directories** (builder.rs). `context_dir` is now
  canonicalized; paths with `..` or absolute components are rejected; symlinks
  are validated to not escape the build context.
- **Blob digest mismatch on sync** (block_sync.rs, sync_puller.rs).
  `Digest::compute(data)` is now performed before `put_blob_blocking`, and
  blobs with mismatched digests are discarded immediately.
- **PodSandboxId returned incorrect value** (runtime_service.go). The gRPC
  `RunPodSandbox` response now returns `req.Config.Metadata.Uid` instead of
  the internal `runResp.Id`.
- **Missing ID validation at gRPC boundary** (store.go). `idRegexp.MatchString(id)`
  is now applied at `RegisterNode` and `DeleteWorkload` to reject malformed IDs.

### High-priority fixes
- **Refcount race condition** (store.rs). `increment_refcount` / `decrement_refcount`
  are now protected by a `refcount_lock` Mutex.
- **`parse_platform()` returned arguments in wrong order** (puller.rs). OS is now
  parsed first, ARCH second, and the function returns `(arch, os)` matching the
  caller's expectation.
- **Control plane API bound to all interfaces** (main.go). Now binds
  `127.0.0.1:8080` and `127.0.0.1:8081` instead of `:8080` / `:8081`.
- **`disable_nat` used hardcoded CIDR** (firewall.rs). Now uses the `bridge_cidr`
  parameter passed by the caller instead of hardcoded `10.42.0.0/16`.

### Medium-priority fixes
- **`watchWindowSize` goroutine leak** (terminal_unix.go, terminal_windows.go).
  The resize watcher now accepts a `stop` channel, selects on it, and the caller
  defers `close(stopWinCh)`.
- **Host `/etc/resolv.conf` leaked into layer tarballs** (builder.rs). The host
  file is now copied into the VM, then removed before the layer scan.

### Initramfs distribution
- Darwin tarballs include `pullrun-initramfs.cpio.gz` (required for Apple
  Virtualization.framework VM backend).
- install.sh downloads initramfs to `~/.pullrun/initramfs/` on darwin.
- Runtime falls back to Homebrew `opt/pullrun/share/pullrun/` paths when the
  user-local path doesn't exist.

## 0.7.5 — 2026-07-26

### Apple Virt VM networking & initramfs delivery

- **VM booted with `eth0` DOWN — `ENETUNREACH` on every network call.** The
  Apple Virt NIC was created by `VZNATNetworkDeviceAttachment` but the Linux
  guest had no DHCP client and no static IP configuration. Added
  `configure_network()` in `pullrun-init` that runs `udhcpc -q -n -T 2 -t 3`
  with a static `192.168.64.2/24` fallback, plus `/etc/resolv.conf` setup.
- **Initramfs missing from release tarballs.** The 1.2 MB initramfs containing
  the network fix was not included in darwin tarballs — fresh installs had no
  initramfs and the VM would fail to boot. Darwin tarballs now include
  `pullrun-initramfs.cpio.gz`.
- **Runtime auto-discovers initramfs at Homebrew paths.** `find_local_kernel()`
  now falls back to `/opt/homebrew/opt/pullrun/share/pullrun/` and
  `/usr/local/opt/pullrun/share/pullrun/` when `~/.pullrun/initramfs/` doesn't
  exist. Fresh `brew install pullrun` works with no manual copy step.
- **install.sh copies initramfs on darwin.** The binary-download path now
  extracts `pullrun-initramfs.cpio.gz` to `~/.pullrun/initramfs/` automatically.
- **Homebrew formula installs initramfs to Cellar + codesigns runtime.**
  `(share/"pullrun").install "pullrun-initramfs.cpio.gz"` places the initramfs
  in the Cellar; `post_install` applies the virtualization entitlement.

## 0.7.4 — 2026-07-25

### Bug fixes
- **Apple Virt PTY: `\r`→`\n` conversion missing in pullrun-init.** `cfmakeraw`
  clears `ICRNL`, so Enter from the host stayed as `\r` and was not recognized
  by Debian-based shells (dash, bash). Added `ICRNL=1` and cleared `ECHOCTL` in
  the guest PTY termios setup. Alpine (BusyBox ash) was unaffected.
- **Startup blocked by synchronous refcount rebuild.** The store refcount rebuild
  (up to ~55s with 7000+ nodes) blocked the gRPC socket bind, causing the CLI
  to time out after 5s. Moved into a background `tokio::spawn`; CLI timeout
  increased to 60s.
- **Homebrew `postinstall` auto-signs runtime binary.** The
  `com.apple.security.virtualization` entitlement is now applied to
  `pullrun-runtime` after every `brew install` / `brew reinstall`, so Apple
  Virtualization VM backend works immediately with no manual codesign step.

## 0.7.3 — 2026-07-24

### Bug fixes
- **exec returned success with pid:0 for container workloads.** `start()` now
  takes `&mut ProcessHandle` so the trait impls can write PID back into the
  handle. After `runc run -d` both rootless and non-rootless executors verify
  the container state via `runc state` and only report success when PID > 0
  and status is `"running"`.

## 0.7.2 — 2026-07-24

### Security
- **quinn-proto updated to v0.11.16 in all Cargo.lock files.** The workspace and two tool crates (apple-virt-smoke, apple-virt-exec) now pin the patched version, closing all 3 Dependabot alerts.

## 0.7.1 — 2026-07-24

### Security
- **grpc-go updated to v1.82.1.** Patches xDS RBAC and HTTP/2 vulnerabilities.
  Applied across all 6 Go modules.
- **quinn-proto updated to v0.11.16.** Patches remote memory exhaustion from
  unbounded out-of-order stream reassembly.

## 0.7.0 — 2026-07-24

### Features
- **Compose auto-build for `build:`-only services.** `pullrun compose up` now
  automatically builds services that have a `build:` section but no `image:`
  field, matching Docker Compose behavior. Previously the puller received an
  empty image ref and failed with UNAUTHORIZED.
- **Compose auto-build for `image:` + `build:` combo.** When both `image:` and
  `build:` are set, the service is built and tagged with the specified image
  name, matching Docker Compose behavior. Previously the puller tried to pull
  the image from the registry.

### Build / packaging
- **install.sh ships `pullrun-compose`.** Added to the binary loop so all three
  binaries (CLI, runtime, compose) are installed from the tarball. `pullrun compose`
  works out of the box with no manual steps.

## 0.6.8 — 2026-07-24

### Bug fixes
- **Rootless `runc` containers now launch correctly.** Fixed three intertwined
  bugs: `--root` flag was passed after the container ID (ignored by runc), spawn
  failures were not checked (daemon proceeded with a dead container), and uid/gid
  mappings + capabilities + mounts were not applied in the rootless config path.
  Rootless mode is now functional on Linux.
- **`pullrun save <tag>` resolves to digest before exporting.** Previously,
  saving by tag produced a silently empty archive. Now `ListImages` resolves the
  tag to a digest; unknown tags produce a clear error.
- **`pullrun rmi` accepts digest prefixes.** Truncated hex prefixes now work —
  the CLI walks the image list and matches the first digest starting with the
  given prefix.
- **`pullrun prune` properly removes exited workloads.** The workload map was not
  cleaned up after container exit, so `rmi` refused to remove the image. Prune
  now removes completed workloads from the map before cleaning up layers.
- **`pullrun build` catches directory-as-Dockerfile.** Passing a directory to
  `--file` now returns a clear error hinting at the correct argument order.
- **`walk_dag_collect` returns errors on invalid digests.** Instead of silently
  using a zero digest and producing a corrupt archive, the DAG export walk now
  propagates the error upward.

### Documentation
- **Binary sizes corrected in README.** Stripped Go binaries are ~14 MB (not
  20 MB); runtime is ~6 MB. Total install size ~20 MB.

## 0.6.7 — 2026-07-22

### Bug fixes
- **Support images with no base layer (`FROM scratch`).** The DAG layer walk now
  handles zero-layer images correctly instead of panicking on an empty iterator.
  `pullrun run scratch-image` works for static binaries and distroless images.
- **`pullrun load` re-roots GC roots.** Manifests loaded from a tar archive now
  register their root digest in the GC root set, preventing immediate garbage
  collection of freshly imported images.
- **OCI-compliant media type detection for Docker-schema manifests.** The
  manifest parser now accepts `application/vnd.docker.distribution.manifest.v1+json`
  (with and without `+prettyjws`) alongside the OCI types. Fixes pull failures
  from registries still serving the legacy Docker format.
- **Port-mapping errors propagate to the caller.** `ensure_mapping` now returns
  the actual error from `proxyd_client` instead of silently swallowing it.
- **Release profile corrected in `Cargo.toml`.** The workspace profile was using
  debug settings for release builds. Now `lto = "fat"` and `opt-level = 3` are
  applied consistently.

### Tests
- **E2E tests using `#[ignore]` for conditional skipping.** Tests that require
  external registries or root privileges are now marked `#[ignore]` with a
  clear reason string, so `cargo test` passes out of the box without special
  setup.

### Documentation
- **Pre-HN polish pass.** CVE severity claims matched to actual CVSS scores;
  benchmarks script published; sweeping claims softened.
- **Zenodo badge replaced.** Badge now uses shields.io (zenodo.org returns 403
  to GitHub's camo proxy).

## 0.6.6 — 2026-07-17

### Bug fixes
- **install.sh consistently uses suffixed binary names.** All three lookup paths
  (macOS/Linux CLI, Windows CLI, WSL2 runtime) now search for `$BIN-$OS-$ARCH`
  first before falling back to bare names. Previously the Windows path looked for
  bare `pullrun.exe` and the WSL2 path looked for bare `pullrun-runtime`, neither
  of which matched the actual tarball contents.

## 0.6.5 — 2026-07-16

### Features
- **CLI `-V` short flag.** Cobra's `Version` field is now registered before
  `InitDefaultVersionFlag()`, so `-V` works instead of the default `-v` (which
  would conflict with `--verbose`).
- **`pullrun-runtime --version`.** The runtime binary now supports
  `--version`/`-V` natively via clap's `#[command(version)]` attribute, instead
  of requiring a separate `version` subcommand.

### Bug fixes
- **install.sh version references are dynamic.** `CURRENT_VERSION` variable at
  the top of the script is referenced in rate-limit error messages instead of
  hardcoded strings.

## 0.6.4 — 2026-07-15

### Bug fixes
- **install.sh binary-selection glob no longer matches wrong binary.** The glob
  `$BIN-*` matched `pullrun-runtime-*` before `pullrun-*` when `BIN=pullrun`
  (alphabetical ordering). Replaced with exact `$BIN-$OS-$ARCH` matching with
  exact basename fallback.

## 0.6.3 — 2026-07-14

### Bug fixes
- **`save`/`load` round-trip digest mismatch.** Removed incorrect blob content
  verification in `dag_import.rs` — the blob path uses the parent node's digest
  (SHA-256 of rkyv-serialized DagNode), not a hash of the raw blob data.
- **`secret ls` / `config ls` CREATED timestamp shows epoch zero.** Added
  `timestamp_from_meta()` helper that falls back to `meta.modified()` when
  `meta.created()` is unavailable (Linux ext4/xfs/btrfs).
- **`pullrun push` error message for unauthenticated pushes.** The CLI now
  detects 401 errors and prints `not authenticated — run 'pullrun login <registry>'`.
- **`pullrun --version` / `-V` flags.** Added cobra `Version: "0.6.2"` to the
  root command. Also added `pullrun-runtime version` subcommand.
- **`install.sh` GitHub API rate-limit handling.** Clear error when rate-limited.
- **`install.sh` non-sudo install path.** Falls back to `~/.local/bin/`.

### Documentation
- README and help text corrections.

## 0.6.2 — 2026-07-13

### Features
- **`pullrun images`** — new CLI command to list pulled images in the local
  DAG store, with table and `--json` output.
- **`pullrun compose` subcommand** — delegates to the `pullrun-compose` binary.

### Fixes
- **`pullrun exec` returns proper NotFound for missing IDs.**
- **`pullrun login` validates credentials before saving.**
- **`pullrun build` resolves paths client-side.**
- **All Dockerfile and flag errors fixed in docs.**

## 0.6.1 — 2026-07-11

### Features
- **nftables support for bridge NAT** (`FirewallBackend` trait).
- **`IptablesBackend`** — ports iptables logic into the trait.
- **`NftablesBackend`** — equivalent rules via `nft -f -`.

### Fixes
- **Sync tests: IPv6 loopback fallback.**

## 0.6.0 — 2026-07-11

### Features
- **DAG store garbage collection** (`pullrun gc`, `pullrun gc --apply`).
- **Crash-atomic writes** with parent `fsync`.
- **Operation locks** protect in-flight operations from GC.
- **`kernel_image_digest` pinning** for VM kernel layers.

### Fixes
- **`enumerate_gc_roots` data-loss bug fixed.**
- **GC correctness fixes** for dry-run, BFS abort on corruption, 90% safety guard.

### Tests
- **25 new tests** for GC mechanics and correctness.
