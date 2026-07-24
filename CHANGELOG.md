# Changelog

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
