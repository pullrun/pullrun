# Release process

## Naming convention

Every file in the release uses the same suffix to identify target platform:

    <binary>-<os>-<arch>

Examples: `pullrun-darwin-arm64`, `pullrun-runtime-linux-amd64`, `pullrun-windows-amd64`.

The tarball wraps the version in too:

    pullrun-<version>-<os>-<arch>.tar.gz

This suffix is **never stripped** — it stays on the binary name inside and
outside the tarball. Consumers (install.sh, Homebrew formula) rename it to
the bare name when placing it on PATH.

| Consumer | How it gets the bare name |
|----------|---------------------------|
| install.sh (macOS/Linux) | `find ... -name "$BIN-$OS-$ARCH"` → copies to `$BIN` |
| install.sh (WSL2) | `find ... -name "pullrun-runtime-linux-$ARCH"` → copies to `pullrun-runtime` inside WSL |
| Homebrew formula | `bin.install "...darwin-arm64" => "pullrun"` (arch block) |

## Prerequisites

Tool                        | Required for
----------------------------|------------------------
Go 1.25+                    | CLI (`cli/pullrun`)
Rust nightly + `aarch64-unknown-linux-musl` target | native + cross Rust
`x86_64-linux-musl-gcc`    | cross-compile Rust → linux/amd64
`aarch64-linux-musl-gcc`   | cross-compile Rust → linux/arm64
`llvm` (macOS)              | cross-compile Rust → x86_64-apple-darwin
`gh` (GitHub CLI)           | create release + upload assets

Install missing musl toolchains on macOS:
```shell
brew install SergioBenitez/osxct/x86_64-unknown-linux-musl \
             SergioBenitez/osxct/aarch64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
```

## Version bump: files to update

| File | Field |
|------|-------|
| `Cargo.toml` | `version = "X.Y.Z"` |
| `cli/pullrun/cmd/info.go` | `fmt.Println("pullrun X.Y.Z")` |
| `cli/pullrun/cmd/root.go` | `Version: "X.Y.Z"` |
| `cli/pullrun/cmd/mcp/mcp.go` | `"X.Y.Z"` |
| `install.sh` | `CURRENT_VERSION="vX.Y.Z"` |

After editing, commit and tag:
```shell
git commit -m "bump version to X.Y.Z"
git tag vX.Y.Z
```

## Build all Go targets

Each binary is named `<binary>-<os>-<arch>`:

```shell
cd cli/pullrun
LDFLAGS="-s -w"  # strip debug info (~20 MB → ~14 MB)

GOOS=darwin  GOARCH=arm64 go build -ldflags="$LDFLAGS" -o /tmp/release/pullrun-darwin-arm64    .
GOOS=darwin  GOARCH=amd64 go build -ldflags="$LDFLAGS" -o /tmp/release/pullrun-darwin-amd64    .
GOOS=linux   GOARCH=arm64 go build -ldflags="$LDFLAGS" -o /tmp/release/pullrun-linux-arm64     .
GOOS=linux   GOARCH=amd64 go build -ldflags="$LDFLAGS" -o /tmp/release/pullrun-linux-amd64     .
GOOS=windows GOARCH=amd64 go build -ldflags="$LDFLAGS" -o /tmp/release/pullrun-windows-amd64   .
```

## Build all Rust targets

Same naming: `<binary>-<os>-<arch>`.

Native (darwin/arm64):
```shell
cargo build --release -p pullrun-runtime
cp target/release/pullrun-runtime /tmp/release/pullrun-runtime-darwin-arm64
```

Cross darwin/amd64:
```shell
CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="/opt/homebrew/opt/llvm/bin/clang" \
CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=-arch -C link-arg=x86_64" \
cargo build --release --target x86_64-apple-darwin -p pullrun-runtime
cp target/x86_64-apple-darwin/release/pullrun-runtime /tmp/release/pullrun-runtime-darwin-amd64
```

Cross linux/amd64:
```shell
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="x86_64-linux-musl-gcc" \
cargo build --release --target x86_64-unknown-linux-musl -p pullrun-runtime
cp target/x86_64-unknown-linux-musl/release/pullrun-runtime /tmp/release/pullrun-runtime-linux-amd64
```

Cross linux/arm64:
```shell
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="aarch64-linux-musl-gcc" \
cargo build --release --target aarch64-unknown-linux-musl -p pullrun-runtime
cp target/aarch64-unknown-linux-musl/release/pullrun-runtime /tmp/release/pullrun-runtime-linux-arm64
```

## Package tarballs

The suffixed binaries go **into** the tarball with their full name (never
stripped). The tarball itself is `pullrun-<version>-<os>-<arch>.tar.gz`.

```shell
cd /tmp/release

tar czf /tmp/pullrun-X.Y.Z-darwin-arm64.tar.gz \
  pullrun-darwin-arm64 pullrun-runtime-darwin-arm64

tar czf /tmp/pullrun-X.Y.Z-darwin-amd64.tar.gz \
  pullrun-darwin-amd64 pullrun-runtime-darwin-amd64

tar czf /tmp/pullrun-X.Y.Z-linux-arm64.tar.gz \
  pullrun-linux-arm64 pullrun-runtime-linux-arm64

tar czf /tmp/pullrun-X.Y.Z-linux-amd64.tar.gz \
  pullrun-linux-amd64 pullrun-runtime-linux-amd64

tar czf /tmp/pullrun-X.Y.Z-windows-amd64.tar.gz \
  pullrun-windows-amd64
```

**Note:** Windows tarball only contains `pullrun.exe` (no runtime — the
runtime is a Linux binary for WSL2 and lives in the Linux tarball).

## Verify the installed binaries

```shell
/tmp/release/pullrun-darwin-arm64 version   # → pullrun X.Y.Z
/tmp/release/pullrun-runtime-darwin-arm64 --version  # → pullrun-runtime X.Y.Z
/tmp/release/pullrun-darwin-arm64 -V        # → pullrun version X.Y.Z
```

Also verify `-V` works on all Go binaries and `--version` on all Rust binaries.

## Release

```shell
git push origin vX.Y.Z && git push

gh release create vX.Y.Z \
  --repo pullrun/pullrun \
  --title "vX.Y.Z — <short description>" \
  --notes "<release notes>" \
  /tmp/pullrun-X.Y.Z-darwin-arm64.tar.gz \
  /tmp/pullrun-X.Y.Z-darwin-amd64.tar.gz \
  /tmp/pullrun-X.Y.Z-linux-amd64.tar.gz \
  /tmp/pullrun-X.Y.Z-linux-arm64.tar.gz \
  /tmp/pullrun-X.Y.Z-windows-amd64.tar.gz
```

## Homebrew formula bump

After release, update the Homebrew tap at `pullrun/homebrew-tap`.

The formula must rename the suffixed binaries to bare names using
`Hardware::CPU.arm?` blocks, because Homebrew expects the bare `pullrun`
and `pullrun-runtime` on PATH:

```ruby
def install
  if Hardware::CPU.arm?
    bin.install "pullrun-darwin-arm64" => "pullrun"
    bin.install "pullrun-runtime-darwin-arm64" => "pullrun-runtime"
  else
    bin.install "pullrun-darwin-amd64" => "pullrun"
    bin.install "pullrun-runtime-darwin-amd64" => "pullrun-runtime"
  end
end
```

Steps:
- Bump `version` to `X.Y.Z`
- Update `url` and `sha256` for both `on_arm` / `on_intel` blocks
- Run `shasum -a 256 /tmp/pullrun-X.Y.Z-darwin-*.tar.gz` for the checksums

## install.sh verification

Test the one-liner in a non-root sandbox (e.g. Docker container or fresh VM):
```shell
curl -fsSL https://raw.githubusercontent.com/pullrun/pullrun/main/install.sh | bash
```

Expected outcome:
- `pullrun` binary exists and is the CLI (not the runtime)
- `pullrun-runtime` binary exists
- Both are in `~/.local/bin` (no sudo) or `/usr/local/bin` (with sudo)
- `pullrun version` prints `pullrun X.Y.Z`
- `pullrun-runtime --version` prints `pullrun-runtime X.Y.Z`

## Earlier critical fixes (context for future agents)

### v0.6.2: save/load digest mismatch
`runtime/pullrun-oci/src/dag_import.rs` — removed blob content re-hashing verification
during `dag_import`. The tar entry path uses the parent node's digest (SHA-256 of
rkyv-serialized `DagNode`), not SHA-256 of raw blob data. Verification by re-hashing
was always incorrect.

### v0.6.2: secret/config CREATED timestamp
`runtime/pullrun-runtime/src/secrets.rs` — added `timestamp_from_meta()` helper that
tries `created()` first, falls back to `modified()` (Linux ext4/xfs/btrfs return Err
from `created()`), then `0` as last resort. Applied to all 4 secret/config list/inspect
paths.

### v0.6.3: install.sh binary-selection glob
`install.sh` — replaced `find -name "$BIN-*"` (which matched `pullrun-runtime-*` before
`pullrun-*` when `BIN=pullrun`) with exact `$BIN-$OS-$ARCH` matching + exact basename
fallback.

### v0.6.4: -V short flag, runtime --version, dynamic install.sh version
- CLI: `cmd.Flags().BoolP("version", "V", ...)` registered before cobra's
  `InitDefaultVersionFlag()` runs, so `-V` works instead of cobra's default `-v`.
- Runtime: `#[command(version)]` on the clap `Cli` parser; removed explicit `Version`
  subcommand. `pullrun-runtime --version`/`-V` now works natively.
- install.sh: `CURRENT_VERSION` variable at top, referenced in rate-limit error
  messages instead of hardcoded strings.

### v0.6.5: consistent suffixed naming across install paths
`install.sh` — all three lookup paths (macOS/Linux, Windows CLI, WSL2 runtime) now
try the suffixed name `$BIN-$OS-$ARCH` first before falling back to bare names.
Previously the Windows path looked for bare `pullrun.exe` and the WSL2 path looked
for bare `pullrun-runtime`, neither of which matched the actual tarball contents.
