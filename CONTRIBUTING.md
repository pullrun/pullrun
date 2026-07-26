# Contributing to Pullrun

Thank you for your interest in Pullrun! We welcome contributions from
everyone.

## Before you start

### 1. Sign the CLA

Before your first pull request can be merged, you must sign our
Contributor License Agreement. See [CLA.md](CLA.md) for details.

### 2. Code of Conduct

We are committed to providing a welcoming and inclusive experience for
everyone. Be respectful, constructive, and assume good faith.

## Getting started

### Prerequisites

- **Rust** 1.78+ — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Go** 1.26+ — `brew install go` or `apt install golang`
- **protoc** (if regenerating protobuf) — `brew install protobuf`

### Building

```bash
# Rust workspace (all crates)
cargo build --workspace

# Go CLI
cd cli/pullrun && go build -o pullrun .

# Everything at once
make build
```

### Testing

```bash
# Rust unit tests
cargo test --workspace

# Go CLI tests
cd cli/pullrun && go test ./...

# Integration tests (require runc + root)
cargo test --workspace -- --include-ignored
```

### Linting

```bash
# Rust
cargo clippy --workspace -- -D warnings
cargo fmt --all --check

# Go
cd cli/pullrun && go vet ./...
```

## Pull request workflow

1. **Fork** the repository and create a branch from `main`
2. **Commit** your changes with clear, descriptive messages
3. **Test** — ensure existing tests pass and add new ones for your changes
4. **Lint** — run clippy and go vet, fix any issues
5. **Open a PR** — describe what you changed and why

### Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add volume mount support for Firecracker VMs
fix: handle EOF correctly in stdin forwarding loop
docs: update detach key section for container backend
```

### What to avoid

- Do not add `eprintln!` or `println!` debug output — use `tracing::info!` / `tracing::debug!`
- Do not commit large binary files, `.env` files, or secrets
- Do not change license headers or the CLA

## Project structure

```
proto/                   Protobuf definitions (single source of truth)
proto-go/                Generated Go protobuf code
runtime/                 Rust workspace: store, oci, exec, net, vm, sync, runtime
cli/pullrun/           Go CLI
cri/pullrun-cri/         Go CRI shim (Kubernetes)
control-plane/           Node registry (stub)
deploy/                  K8s manifests, Grafana dashboard
tools/                   Standalone tools
docs/                    Documentation
```

## Getting help

- Open an issue for bugs or feature requests
- Tag maintainers in your PR for review

Thank you for contributing!
