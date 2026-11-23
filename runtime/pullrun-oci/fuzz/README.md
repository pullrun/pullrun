# Fuzzing `pullrun-oci`

The highest-value fuzz target is the layer-tar path parser in
`src/confinement.rs` (`RelPath::parse`), which handles attacker-controlled
entry names.

```bash
cd runtime/pullrun-oci
cargo +nightly fuzz run relpath -- -max_total_time=300
```

The target asserts that absolute paths and `..` components are always
rejected and that safe relative paths are accepted. Run it on Linux with
ASAN in CI/nightly; the macOS ASAN runner used during development hung
before executing iterations, so treat the target as unverified on macOS.
