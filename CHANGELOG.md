# Changelog

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
