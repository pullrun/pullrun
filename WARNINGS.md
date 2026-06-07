# Nimbus — Development Warnings & Gotchas

> **Read this before resuming work.** This file documents non-obvious pitfalls
> discovered during development that have caused or could cause real damage.

---

## ⚠️ Process warning: when something "looks like" a known issue but isn't, STOP and add observability

**Severity: HIGH — this is the meta-cause of most of our wasted debugging time.**

### The pattern

We repeatedly burned 30-120 minutes on false positives because the
symptoms "looked like" something we'd already hit:

| Symptom                                                  | False-positive diagnosis we chased | Actual cause |
|----------------------------------------------------------|------------------------------------|--------------|
| `spawn /bin/sh: No such file or directory` in guest     | "the cpio symlinks are broken"     | `Command::current_dir("")` in nimbus-init |
| `vsock read EOF on header` after writing WorkloadSpec   | "the framework's listener is on the wrong thread" | (it was — but we fixed that, and the EOF was then from a different cause: the `current_dir` bug above) |
| `AF_VSOCK connect failed; trying Unix fallback`         | "the host's listener isn't set up"  | (it was the first time we hit it; on subsequent runs it was a side-effect of the guest having already crashed once and the kernel panicking) |
| `vm.state() returns 0 instead of 1` after start         | "the framework's queue is wrong"    | we were reading the state value from the wrong thread |
| InitHello received but WorkloadSpec response is EOF     | "the framework's connection got reset" | (it did — but the *reason* was that the guest's spawn failed, nimbus-init exited, the kernel panicked, and the framework's state was now weird) |

### The rule

**When you have a hypothesis and the symptoms match, but the
evidence contradicts it (the file is clearly there, the
framework call returns the right value, the kernel log shows
the init script reached the right line), STOP. You are
probably looking at a different bug that has the same shape.**

Concretely:

1. **Add a virtio console to the VM as your FIRST debugging tool.**
   Don't read code for an hour. Add the console device, get
   dmesg + init logs into a host file, and read them. The
   first time we did this we found the `spawn /bin/sh: No
   such file or directory` immediately, but only after we'd
   already wasted an hour on cpio symlink formats.
2. **Add `tracing::info!` / `eprintln!` to the GUEST** (nimbus-init)
   so you can see what it sees. The kernel log is great for
   the kernel, but it doesn't tell you what user-space is
   doing.
3. **Verify the file/connection/handle exists from the same
   process that's about to use it.** We spent time
   investigating "is /bin/sh really in the cpio" when we
   should have run `stat` from inside the guest and confirmed
   it (which we eventually did, and it was always there).
4. **If a fix doesn't make the symptom go away, the
   hypothesis was wrong.** Don't compound fixes; revert and
   re-hypothesize.

### Specific things to NOT do

- **Don't** read `vm.state()` from a body thread. (It traps.
  See the Apple Virt section.)
- **Don't** write a custom `DispatchQueue` for the VM. (It
  asserts on completion delivery. Use `DispatchQueue::main()`.)
- **Don't** call `Command::current_dir("")`. (It makes spawn
  return `ENOENT` for the binary.)
- **Don't** add eprintln-style debug logs that are conditional
  on "I just made a change" and forget to remove them. We
  left a `2-second sleep` in the WorkloadSpec write path
  that we forgot about and which masked the real bug for
  a few runs.
- **Don't** assume the cpio is wrong because a file can't be
  found. Verify with `stat` first.

---

## ⚠️ DO NOT use `github.com/nimbus/nimbus` (or any `github.com/nimbus/*`) as a Go module path

**Severity:** HIGH — this has caused real-world confusion in this project.

### What happened

`github.com/nimbus/nimbus` is the path of an **unrelated, real, existing project
on GitHub**. It is not our project. Our project lives at `/Users/YACINE/nimbus`
locally and has no canonical public home.

When our proto files declared:

```proto
option go_package = "github.com/nimbus/nimbus/control-plane/api/proto/.../runtime;runtime";
```

…and our Go modules were named `module github.com/nimbus/nimbus/...`, several
bad things happened:

1. `go mod tidy` resolved `github.com/nimbus/nimbus` from the public module
   proxy and pulled down `v0.1.33` of the *real* nimbus project — silently
   shadowing any work-in-progress in this repo.
2. Generated `.pb.go` files claimed import paths that, to a tool or human
   outside this machine, point to a public project they don't belong to.
3. The risk of pushing any of this code to a public remote and having it
   collide with — or be mistaken for — the real `github.com/nimbus/nimbus` is
   real and would be a public-relations disaster.

### The rule

**Never use a `github.com/...` path for any module, proto `go_package`, or
generated stub in this project unless this project actually lives at that
GitHub URL.** This project does not. Until a real public home is chosen:

- **Module paths:** use `nimbus/...` (e.g. `nimbus/cli`, `nimbus/controlplane`).
  `go.mod` does not require a domain; the path can be any unique string.
- **Proto `go_package`:** use `nimbus/<subdir>/proto/...` paths that match
  where the generated stubs actually live in this repo.
- **Never prefix with `github.com/`, `gitlab.com/`, `bitbucket.org/`, or any
  other real forge.** Use a path that is clearly local (e.g. just `nimbus/...`)
  until a canonical home is decided.

### How to spot this mistake

```bash
# Should print nothing:
rg -n 'github\.com/(nimbus|nimbus-)' .
rg -n '"go_package":\s*"github\.com/' proto/
```

If either command prints anything, the mistake has been re-introduced and
must be fixed before continuing.

### Current canonical scheme

| Module                              | Path in repo                 |
| ----------------------------------- | ---------------------------- |
| `nimbus`                            | (root, used as a prefix only)|
| `nimbus/cli`                        | `cli/nimbusctl/`             |
| `nimbus/controlplane`               | `control-plane/api/`         |
| `nimbus/cri`                        | `cri/nimbus-cri/`            |

Proto `go_package` for `nimbus/runtime.proto`:

```
nimbus/controlplane/proto/nimbus/runtime;runtime
```

Consumers (`nimbusctl`, `nimbus-cri`) use a `replace` directive in their
`go.mod` to point at a local copy of the generated stubs.

---

## Other pitfalls

### `protoc-gen-go` output layout

- `paths=source_relative` puts all `.pb.go` files in a single dir, which
  collides when each proto has a different `go_package` name.
- `paths=import` honors the `go_package` and writes each proto to its own
  subdir, but mirrors the import path under the output dir. Always follow
  `protoc` with a `mv` step to flatten the mirror.

### pflag v1.0.5 + cobra v1.8.0

- `Uint16SliceVar`, `Uint32SliceVar`, `Uint64SliceVar` **do not exist** on
  `*pflag.FlagSet` in pflag v1.0.5. Use `StringSliceVar` and parse the
  values manually if you need a typed slice flag.

### `genproto` ambiguity

- `k8s.io/cri-api v0.27.1` transitively pulls in an old
  `google.golang.org/genproto@v0.0.0-20220502173005-c8bf987b8c21` that
  conflicts with the newer `genproto/googleapis/rpc` v0.0.0-20240318...
  used by gRPC v1.64. Add an `exclude` directive in `go.mod`:
  `exclude google.golang.org/genproto v0.0.0-20220502173005-c8bf987b8c21`.

### `tokio::process::Child::id()` returns `u32`, not `Option<u32>`

- The `Option<u32>` API is `std::process::Child`. Tokio's wrapper returns
  the bare `u32` (0 if the OS hasn't reported the pid yet). Don't
  `if let Some(pid) = child.id()` — it won't compile.

### TAP device creation via `ioctl(TUNSETIFF)` (replaces `ip tuntap add`)

- Nimbus creates TAP devices via direct `ioctl(TUNSETIFF)` on `/dev/net/tun`,
  NOT via the `ip tuntap add` subprocess. This eliminates the need for
  ambient capabilities.
- The binary must have `setcap cap_net_admin=eip` for rootless operation.
- **The TAP device lives only as long as the `/dev/net/tun` file descriptor
  is open.** Closing the fd destroys the device. Hold the fd in the caller
  for the VM's lifetime. `FirecrackerExecutor::tap_fds` is the canonical
  pattern.
- The `ifreq` struct passed to the kernel MUST be `sizeof(struct ifreq) = 40`
  bytes. An undersized struct (e.g. 18 bytes) causes the kernel to read
  garbage past the boundary; the ioctl returns 0 but the device never
  appears. Include a `_pad: [u8; 22]` field after `ifr_flags`.
- `TUNSETIFF` = `_IOW('T', 202, int)` = `0x400454CA` on x86_64 Linux.
- The legacy `ip tuntap add dev <name> mode tap` and `ip tuntap add name
  <name> mode tap` syntax is **no longer used by Nimbus**. The iproute2
  note below is kept for reference in case manual TAP creation is needed
  during debugging.

### Linux bridge kernel dataplane: `ip link add type bridge` needs no special perms

- `ip link add name nimbus-br0 type bridge` works in any container with
  `CAP_NET_ADMIN`. Same for `ip addr add 10.42.0.1/16 dev nimbus-br0`.
- `ip link set <tap> master nimbus-br0` requires `CAP_NET_ADMIN` and a
  kernel that supports bridge netfilter; if the guest has no network,
  check `dmesg | tail` on the host for `bridge: filtering` errors.
- IPv4 forwarding (`/proc/sys/net/ipv4/ip_forward = 1`) is best-effort:
  some sandboxes disallow writing to `/proc/sys`. We treat that as a
  warning, not a fatal error.

### Linux ifname max length is 15 chars

- `IFNAMSIZ = 16` (15 chars + NUL). Any ifname longer than 15 chars
  is rejected with `ENAMETOOLONG` or, via iproute2, with a misleading
  `"name" not a valid ifname` error.
- This bit us on a `tap-nimbus-test-1` (17 chars) probe. Use
  short names: `tap-np`, `tap-vm-test`, `tap-{id8}`.
- For workload-specific tap devices, use a 12-char hex hash of the
  workload id (as `tap_name_for` does in `nimbus-vm/src/lib.rs`).

### Firecracker `--log-path` requires the target file to exist

- Firecracker opens the file with `O_TRUNC` but does **not** create the
  parent directory or the file itself. Always `touch` the log path
  before spawning the process, or you get
  `Failed to open target file: os error 2` and firecracker exits
  immediately.
- Apply the same `touch` to the API socket's parent dir; `UnixListener`
  bind will fail with `ENOENT` otherwise.

### Firecracker guest serial is on stdout/stderr, not the `--log-path`

- `--log-path` captures **only** the firecracker process's own log lines.
- The guest kernel's `console=ttyS0` output goes to firecracker's
  stdout/stderr. Pump both streams to files and `tail -F` all three
  sources (log + serial.out + serial.err) when looking for boot
  markers.
- For full serial capture in the VMM, set up a `vsock` / socket-based
  console handler via the API; that's beyond v0.

### Kernel `ip=` boot arg format

- Format: `ip=<client-ip>::<gw-ip>:<netmask>:<hostname>:<iface>:<config>`
- Set `hostname=` (empty), `config=off` to disable DHCP and BOOTP.
- The `iface` field must match the name in `network-interfaces[].iface_id`
  (we use `eth0`).
- Without the trailing `:off`, the kernel falls back to DHCP, which will
  time out if there's no DHCP server on the bridge.
- A working example:
  `ip=10.42.0.5::10.42.0.1:255.255.0.0::eth0:off`

### `iptables` MASQUERADE for VM outbound NAT

- `nimbus-vm::network::enable_nat()` installs three rules. Each is
  checked first with `iptables -C` and only appended with `-A` if
  absent, so the call is safe to invoke on every VM boot.
- The rules are:
  1. `iptables -t nat -A POSTROUTING -s 10.42.0.0/16 ! -d 10.42.0.0/16
     -o <outbound_iface> -j MASQUERADE`
  2. `iptables -A FORWARD -i nimbus-br0 -o <outbound_iface> -j ACCEPT`
  3. `iptables -A FORWARD -i <outbound_iface> -o nimbus-br0 -m state
     --state RELATED,ESTABLISHED -j ACCEPT`
- `<outbound_iface>` is the first `dev <iface>` token from
  `ip route show default`. If parsing fails (no default route), we log
  a warning and skip NAT install — inbound networking still works.
- Best-effort: also writes `/proc/sys/net/ipv4/ip_forward = 1`.
  Some sandboxes disallow writing to `/proc/sys`; treated as a warning.
- `iptables` must be on PATH; the function returns
  `VmNetError::IptablesNotFound` if it isn't. Smoke tests skip-fast
  in that case (they don't pretend to be a "pass").
- **`v0` outbound policy stance**: bridge-level MASQUERADE allows all
  egress. Declared `NetworkRule::Outbound` rules are tracked in the
  workload spec for future enforcement but **not** enforced for raw
  TCP in v0. v1 will add an HTTP-CONNECT proxy on
  `10.42.0.1:3128` for HTTP/HTTPS and either per-VM nftables cgroup
  rules or an outbound SOCKS proxy for raw TCP.
- If iptables-persistent isn't installed, the rules don't survive a
  host reboot. The next Nimbus VM boot will re-install them
  (idempotent), but other workloads on the bridge lose outbound until
  then. Fix: `apt install iptables-persistent` and `netfilter-persistent save`.
- Verify rules after boot with:
  `iptables -t nat -L POSTROUTING -n -v` (look for the
  `10.42.0.0/16` source match) and
  `iptables -L FORWARD -n -v` (look for the bridge↔iface pairs).

### `metrics-exporter-prometheus` and the global recorder singleton

- The `metrics` crate allows exactly **one** global recorder per
  process. A second `install_recorder()` call returns
  `SetRecorderError::AlreadySet`. The naive fix (`.unwrap_or_else(|_| ...)`)
  panics in the fallback if the closure also tries to install.
- Use `OnceLock::get_or_init` for the install path so concurrent
  callers all receive a clone of the same handle and only the
  *first* call does the actual install. The `metrics::install_recorder()`
  function in `nimbus-runtime/src/metrics.rs` is the canonical
  pattern; copy it, don't reinvent.
- For tests: don't `set_global_default` directly. Always go through
  the `install_recorder()` wrapper, and write the test so that
  increment/render assertions come *after* one call to
  `install_recorder()`. The libtest default is `--test-threads=N`
  with N>1, so the install call has to be reentrant.
- `metrics-exporter-prometheus` will not emit a counter or gauge
  series until at least one observation has been recorded. A
  freshly-started daemon's `/metrics` will show only `nimbus_build_info`
  and the `nimbus_store_*` gauges (which are written once at install
  and every 60s thereafter). Don't panic when the first scrape
  doesn't include `nimbus_pulls_total` — the counter simply hasn't
  fired yet.

### `metrics-exporter-prometheus` default features pull in protobuf

- The crate's `default-features = true` enables a `protobuf` feature
  for the OpenMetrics protobuf format. We don't need it (Prometheus
  scrapes the text format, which is the default). Add
  `default-features = false` to the dep entry:
  `metrics-exporter-prometheus = { version = "0.15", default-features = false }`.
  This saves ~30 transitive deps and a meaningful build time on
  disk-constrained CI hosts.

### `--metrics-addr` flag value parsing: `num_args = 0..=1`

- `clap` needs an explicit `num_args = 0..=1` plus
  `default_missing_value = "127.0.0.1:9090"` for the bare-flag
  case (`--metrics-addr` with no value) to work. Without it, the
  user gets `error: a value is required for '--metrics-addr <ADDR>'`.
- The `value_name = "ADDR"` is what makes the help text readable.
  `Option<SocketAddr>` is the right type — clap parses the string
  into a `SocketAddr` at the type level. An invalid address fails
  with a clear `InvalidSocketAddr` error before `main` runs.

### Prometheus histogram bucket choice

- The default `metrics-exporter-prometheus` histogram buckets are
  exponential, which is wrong for our latencies: pulls are
  sub-second to a few seconds (network bound), workload starts
  are sub-second to ~10s (container/VM spawn).
- Set explicit buckets with
  `PrometheusBuilder::set_buckets_for_metric(Matcher::Full(name), &[...])`
  to get useful quantile summaries. The defaults would force a
  `0.001..1000s` range with mostly-empty buckets.
- Buckets must be sorted ascending; the exporter will reject
  unsorted slices silently (the bucket setup just returns Err and
  we fall back to defaults — look for the
  "could not set histogram buckets; using default" warning in the
  daemon log if quantile queries look weird).

### Inspect/events gRPC method must be declared in the proto file

- Adding a Rust method to the `impl Runtime for RuntimeService`
  block is **not enough** — `tonic-build` regenerates the trait
  at compile time from `proto/nimbus/runtime.proto`. If the
  method is missing from the proto, you'll get a confusing
  `error[E0407]: method X is not a member of trait Runtime`.
- The build error is misleading: the trait is generated, but
  the Rust code in `service.rs` is implementing methods that
  aren't on the trait. Always edit both: add `rpc Foo(FooRequest)
  returns (FooResponse);` to the proto and re-run `make proto`
  (or just `cargo build`, which triggers `tonic-build`).

### Shared state via `Arc<RwLock<...>>` and the deref trick

- The gRPC handlers receive `&self` and the background
  exit-watcher task needs to poll the same `workloads` map. We
  share the map by changing `RuntimeService.workloads` from
  `RwLock<HashMap<...>>` to `Arc<RwLock<HashMap<...>>>`.
- The existing handler call sites `self.workloads.write().await`
  keep working because `Arc<T>` derefs to `T` and the method
  resolver autoderes through `Arc` to find `RwLock::write`.
- Do *not* create a second non-Arc copy in the service
  constructor and try to share it with a "watcher" field —
  updates via the handlers would not be visible to the watcher.
  This was the original mistake and led to a watcher that
  silently saw no workloads.

### `tokio::sync::broadcast` Lagged error means events were dropped

- `broadcast::Receiver::recv` returns `Err(RecvError::Lagged(n))`
  when the receiver fell behind and the channel rolled over. We
  log a `warn!` and continue; we do *not* surface this to the
  gRPC client (would require an extension on the proto).
- The fix in the v0 design is "make the consumer faster or
  increase the channel capacity" — capacity is 1024, which gives
  ~100 KiB max memory and is enough for the CLI follow use
  case.
- A future persistent event WAL (not in v0) would give
  at-least-once semantics to audit consumers.

### `Event::into` for proto conversion lives in `lib.rs`, not `events.rs`

- `events.rs` should not import the proto types — it would
  create a cycle (proto is generated by `tonic-build` at
  compile time, and events is included by both lib and the
  service).
- The `From<Event> for proto::Event` impl is defined in
  `lib.rs`, where both `Event` and `proto::Event` are in scope.
  This is the standard Rust pattern: keep conversion traits in
  the module that depends on both types.

---

## Apple Virtualization framework (`objc2` 0.6 + `objc2-virtualization` 0.3) — gotchas hit

### `dispatch_main()` MUST be called from the main thread (deadlocks entire process otherwise)

- **Apple's docs say this explicitly**, and it's not lenient. Calling
  `dispatch_main()` from a non-main thread causes the **entire process**
  to silently deadlock: `std::thread::sleep()` stops waking up,
  `Once::call_once` never returns, and the main thread is effectively
  frozen even though it's not blocked on the dispatch queue.
- This is because `dispatch_main()` waits on a Mach port that is owned
  by the main thread's runloop. When called from a non-main thread, the
  Mach port is never signaled (or is signaled to a thread that never
  pumps it), and the entire process hangs.
- **The implication for tokio-based runtimes:** `#[tokio::main]` owns the
  main thread for the async executor, which is NOT running a libdispatch
  runloop. You CANNOT call `dispatch_main()` on the main thread if it
  also runs tokio. The only fix is:
  1. Spawn a side thread that runs the tokio runtime (with
     `Runtime::block_on`).
  2. On the main thread, call `dispatch_main()`.
  3. The side thread uses `tokio::task::spawn_blocking` → Apple Virt FFI,
     which dispatches completion handlers to the main queue.
- For the `nimbus-runtime` daemon, this means:
  - `main()` is a plain function (not `#[tokio::main]`).
  - macOS: side thread runs tokio, main thread parks on `dispatch_main()`.
  - Linux: tokio stays on the main thread (no Apple Virt).
  - One-shot subcommands (pull, run, stop, list) use a current-thread
    tokio runtime via `block_on` on the main thread — they don't need
    `dispatch_main()` because they don't boot Apple Virt VMs.

### `libdispatch` assertion crash (exit 133 / SIGTRAP) when calling framework APIs off the wrong queue

- `VZVirtualMachine::initWithConfiguration:` (the single-arg init that
  uses a private serial queue) and any completion handler delivery MUST
  happen on the VM's configured dispatch queue.
- Calling `initWithConfiguration_queue` with a custom (non-main) queue
  causes the framework to assert when the completion handler is later
  delivered on that queue — exit 133 (SIGTRAP from libdispatch's
  `dispatch_assert_queue_fail`).
- Calling `startWithCompletionHandler:` (or any other framework method
  with a completion block) from a background thread on a queue that
  isn't the VM's queue ALSO traps.
- **The fix is always the same:**
  1. Create the VM with `initWithConfiguration_queue` and
     `DispatchQueue::main()`.
  2. The main thread calls `dispatch2::DispatchQueue::main().exec_main()`
     (or just runs `dispatch_main()`) to pump the main queue.
  3. Any framework call that needs to be on the queue is dispatched via
     `vm_queue.exec_async(closure)`; completion handlers delivered on
     the queue then run on the main thread.
- **Never** read `vm.state()` or `vm.canStart()` from a non-queue
  thread. Property reads on the wrong thread trap. (We observed
  SIGTRAP on `canStart()` in the body thread before removing the
  pre-flight check.)
- **Never** use a private `DispatchQueue::new()` or any custom
  `DispatchQueue` for the VM. Only `DispatchQueue::main()` works
  reliably.
- The panic hook in the binary's `main` should call
  `libc::_exit(1)` (NOT `std::process::exit`), and the worker's
  exit path should also call `libc::_exit(code)`. Otherwise the
  panic unwind runs the framework's `Drop` glue on the wrong thread
  and traps.

### `VZVirtualMachineState` vtable: 1 = Running, NOT 4

- The state values in Apple's docs are 0-indexed by display order,
  not by raw value. The actual `VZVirtualMachineState` values are:
  - 0 = Stopped
  - 1 = **Running** ← what we use
  - 2 = Paused
  - 3 = Error
  - 4 = Starting
  - 5 = Pausing
  - 6 = Resuming
  - 7 = Stopping
  - 8 = Saving
  - 9 = Restoring
- An earlier note in this repo (and in some blog posts) had 4 as
  "Running" — that's wrong. We hit this and lost a few minutes
  debugging a "VM won't reach state Running" mystery.

### `VZVirtioSocketListener.setSocketListener_forPort:` and `VZVirtioSocketConnection.fileDescriptor` I/O MUST happen on the VM's queue

- Setting the socket listener on a port and reading/writing the
  connection's file descriptor from a body thread (not the VM's
  configured queue) triggers a libdispatch assertion.
- The exact symptom: the binary exits with 133 / SIGTRAP, no Rust
  panic, no useful error message.
- The workaround is the same as above: dispatch the call to the
  main queue with `vm_queue.exec_async(closure)`, then wait on a
  `mpsc::channel()` to know it completed.

### Apple's virtio vsock fd may not be safe for `poll(2)` from a separate reader thread

- We tried a "reader thread polls the fd, writer thread does the
  spec write" pattern and saw the read dup observe `POLLIN` then
  read 0 bytes (EOF) immediately after the write. The framework's
  vsock transport seems to have ordering constraints: the read
  dup can observe EOF if the write dup's bytes are still in flight.
- The simpler pattern — a single thread that does the write, then
  the read, sequentially — works reliably. Use that for the v0
  smoke-test path. A proper bidirectional stream pump (with a
  read thread + a write thread) needs more careful design; we
  punted on that for v0.

### VZVirtioSocketConnection's `fileDescriptor` may be non-blocking by default; explicitly set it blocking before `read_exact`

- `connection.fileDescriptor()` returns a single fd that's used for
  both directions. It may inherit `O_NONBLOCK` from the framework.
  `std::io::Read::read_exact` on a non-blocking fd returns
  `ErrorKind::WouldBlock` instead of blocking, which the host
  code path interpreted as EOF.
- The fix: `libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK)`
  on the dup'd fd before any blocking read. (`read_init_hello_blocking`
  in `runtime/nimbus-vm/src/apple/attach.rs` does this.)

### `VZVirtioSocketConnectionDelegate` connection callback signature: `connection:` is the third arg, not the second

- The Objective-C method is:
  ```objc
  - (BOOL)listener:(VZVirtioSocketListener *)listener
      shouldAcceptNewConnection:(VZVirtioSocketConnection *)connection
      fromSocketDevice:(VZVirtioSocketDevice *)socketDevice;
  ```
- In `objc2-virtualization` 0.3.2, the rust method is named
  `listener_shouldAcceptNewConnection_fromSocketDevice` (the
  `connection:` arg is collapsed into the method name as a
  positional/label). The connection is the second arg, the device
  is the third.
- The delegate should save the connection via `Retained::retain` to
  keep it alive. Returning `true` from `shouldAcceptNewConnection`
  WITHOUT retaining the connection drops the connection immediately
  — the framework has no other way to hand it to you.

### `setSocketListener_forPort:` is asynchronous; first connect from the guest can race

- `setSocketListener_forPort:` returns immediately. The actual
  listener registration happens on the VM's queue, possibly
  after the call has returned. If the guest's vsock connect
  arrives in the gap, the connection is reset (we observed
  `ECONNRESET` (errno 104) on the guest's `connect(2)`).
- **Per-VM attach resolves the 2nd-boot case**: the warm-pool
  scenario where a kernel panics and reboots into a second
  nimbus-init is no longer relevant — each workload gets a
  fresh VM. The first-connect race is mitigated by the 5×200ms
  guest-side retry.
- The guest-side fix is to retry `connect()` with a short
  backoff (we use 5 attempts × 200ms).
- The host-side fix is to register the listener as early as
  possible (before starting the VM) and to wait for an
  "is ready" signal from the framework if one exists.

### Need to add a virtio-console device to see kernel + init logs

- The default kernel cmdline has `console=hvc0` but no
  `VZVirtioConsoleDeviceConfiguration` is configured, so the
  kernel has nowhere to write its `printk` output.
- Symptom: when the guest hangs or panics, the host has no
  visibility into what happened. We've spent hours chasing
  "command not found" / "connect failed" / "spawn ENOENT"
  mysteries that turned out to be obvious in the kernel log
  — once we had a console device.
- Add a `VZVirtioConsoleDeviceConfiguration` with a single
  `VZVirtioConsolePortConfiguration` (set `isConsole=true`)
  attached to a host file via `VZFileHandleSerialPortAttachment`
  (write → log file, read → /dev/null). See
  `runtime/nimbus-vm/src/apple/attach.rs:build_attach_vm_config`
  for the working implementation.

### `VZVirtioConsoleDeviceConfiguration.setPorts:` doesn't exist; use the indexed-subscript pattern

- The framework doesn't expose a `setPorts:` method on
  `VZVirtioConsoleDeviceConfiguration`; you get the existing
  port array via `.ports()` and set individual entries via
  `setObject:atIndexedSubscript:`.
- The Cargo features you need are `VZConsoleDeviceConfiguration`,
  `VZVirtioConsolePortConfiguration`,
  `VZVirtioConsolePortConfigurationArray`, `VZConsolePortConfiguration`,
  `VZSerialPortAttachment`, `VZFileHandleSerialPortAttachment`. Plus
  `NSFileHandle` and `NSFileManager` from `objc2-foundation`.

### `NSFileHandle::initWithFileDescriptor:` requires a `RawFd` (`c_int`), not a `c_char *`; use the class methods for files

- `initWithFileDescriptor:closeOnDealloc:` takes a single `c_int`.
  We tried to use it after `libc::open(path, ...)` and the
  trait-bound `Message` impl wasn't satisfied — the type
  requires a `cfg(feature = "NSObject")` on `objc2-foundation`
  plus a non-`Allocated<NSFileHandle>` receiver.
- The much simpler alternative is the class method
  `NSFileHandle::fileHandleForWritingAtPath(&NSString) -> Option<Retained<NSFileHandle>>`
  (and the matching `fileHandleForReadingAtPath:`). These just
  work with the existing feature gates and don't require any
  raw fd juggling.

### `libc` must be a `cfg(target_os = "macos")` dep, not just `aarch64-apple-darwin`

- Adding `libc = "0.2"` only under `[target.aarch64-apple-darwin.dependencies]`
  means it doesn't pull in for `cargo check` on x86_64 macOS, but
  the FFI code paths that need it (`attach.rs` et al) are
  `#[cfg(target_os = "macos")]` so they'll fail to compile in tests
  on any non-macOS target. Just put it under
  `[target.'cfg(target_os = "macos")'.dependencies]`.

### Don't use a separate reader thread + dup'd fd; use a single fd in the same thread

- We initially dup'd the vsock fd into a read half and a write
  half, then ran a reader thread that `poll(2)`-ed the read half.
  The framework's vsock transport doesn't seem to support
  concurrent reads + writes from different threads on dup'd
  fds — the reader thread observed spurious EOF after the writer
  thread sent bytes.
- The simpler pattern (single thread, one fd, sequential read
  after write) is reliable. We adopted that for v0.

### Don't run a "pre-flight" `canStart()` before `startWithCompletionHandler:`

- We had a flow that called `vm.canStart()` first to check
  whether the VM was bootable, then called `startWithCompletionHandler:`.
  `canStart()` from a non-queue thread (or even from the queue but
  before the VM has fully configured) traps.
- The framework's completion handler reports the same error (or
  success) as `canStart()` would. Just call `startWithCompletionHandler:`
  and handle the error in the completion block.

### `Retained::retain` returns `Option<Retained<T>>`, not `Retained<T>`

- The signature is `pub unsafe fn retain(ptr: *mut T) -> Option<Retained<T>>`.
- It returns `None` if the pointer is null. Wrapping a non-null
  pointer in `Retained` to keep it alive should be:
  ```rust
  *guard = Retained::retain(conn_ptr).expect("non-null connection");
  ```
- We initially wrote `*guard = Retained::retain(conn_ptr);` and
  the type inference picked the wrong overload, leading to a
  confusing `Option<Retained<VZVirtioSocketConnection>>` vs
  `Retained<...>` mismatch.

### `Message` trait is at `objc2::Message`, not `objc2::runtime::Message`

- The `Message` trait in `objc2::runtime` is private; it is
  re-exported at the crate root as `objc2::Message`. Same for
  `ClassType` — use the top-level path.
- `AnyClass`/`AnyObject`/`NSObject` are in
  `objc2::__framework_prelude` (private), but in practice you
  don't need them directly — `objc2::Message` + `objc2::ClassType`
  is enough for typical `Retained` work.

### `block2` is not re-exported by `objc2` — add it as a direct dep

- `objc2-virtualization` uses `block2::DynBlock<dyn Fn(*mut NSError)>`
  for completion handlers, but the crate does NOT re-export
  `block2` itself.
- Add `block2 = "0.6"` to your `Cargo.toml`'s
  `[target.'cfg(target_os = "macos")'.dependencies]` block.
- Then `use block2::RcBlock;` directly. `RcBlock::new(closure)`
  produces a `RcBlock<dyn Fn(...)>` that auto-derefs to
  `&DynBlock<dyn Fn(...)>` for the FFI signature.

### `Retained::default()` does not work for `VZVirtualMachine` (or any other `extern_class!` class without an explicit `DefaultRetained` impl)

- The `extern_class!` macro does NOT auto-impl `DefaultRetained`.
- `Retained::default()` only works for `NSObject` and a few
  others.
- For all `init`-style constructors, use the pattern:
  ```rust
  let allocated: Allocated<T> = unsafe { msg_send![T::class(), alloc] };
  let obj = unsafe { T::init(allocated, ...) };
  ```
  Or, with a `T::class()` helper:
  ```rust
  fn alloc_objc<T: ClassType>() -> Allocated<T> {
      let cls = T::class();
      unsafe { msg_send![cls, alloc] }
  }
  ```

### `validate` is `validateWithError:` and returns `Result<(), Retained<NSError>>`

- The Objective-C method is `validateWithError:`, NOT `validate`.
- The Rust binding returns `Result<(), Retained<NSError>>`,
  not `bool` and not `Option<...>`.
- Always:
  ```rust
  let result: Result<(), Retained<NSError>> = unsafe { vm_config.validateWithError() };
  if let Err(err) = result {
      let description = err.localizedDescription();
      return Err(AppleVirtError::InvalidConfig(description.to_string()));
  }
  ```
- The error is auto-released by the framework; you don't need
  to call `release()` on it explicitly.

### `Retained<VZVirtualMachine>` is `!Send` (and `!Sync`)

- The class contains an internal `PhantomPinned` and the
  `AnyObject` type holds raw `*const UnsafeCell<()>` that does
  not implement `Send`.
- You **cannot** move a `Retained<VZVirtualMachine>` into
  `std::thread::spawn` or `tokio::spawn`. The compiler will
  tell you so.
- Consequences for `Drop`: you cannot asynchronously stop the
  VM in a background thread. Options:
  1. **Document `release().await?` as mandatory** and let
     `Drop` be a no-op (with a `warn!` log). When the
     `Retained` is dropped, the framework releases the
     Objective-C refcount and reclaims the VM on dealloc.
     The kernel may keep running until process exit but no
     memory or file descriptors leak.
  2. **Block the current thread** in `Drop` by calling
     `stopWithCompletionHandler` synchronously (using
     `RcBlock` + `recv_timeout`). This will block the async
     executor for up to `CALLBACK_TIMEOUT`. Acceptable for
     shutdown paths, NOT for the hot path.
- v0 of this project chose option (1).

### `isSupported()` and `state()` require explicit `unsafe {}` blocks

- Auto-marked `unsafe` in `objc2-virtualization` 0.3.2
  because the underlying API is part of the system library
  and the binding doesn't prove safety statically.
- Wrap each call in `unsafe { ... }`:
  ```rust
  if unsafe { VZVirtualMachine::isSupported() } { ... }
  unsafe { self.vm().state() }
  ```

### `VZGenericPlatformConfiguration::new()`, `VZVirtualMachineConfiguration::new()`, `VZNATNetworkDeviceAttachment::new()` are unsafe

- The `new` constructors for the three top-level config
  objects are `unsafe fn` in `objc2-virtualization` 0.3.2.
- Wrap each call in `unsafe { ... }`.

### `NSArray::from_retained_slice` works for parent-class coercion via `Retained::into_super`

- When you have a `Retained<Sub>` and need an
  `NSArray<Super>` (e.g. `VZVirtioFileSystemDeviceConfiguration`
  → `VZDirectorySharingDeviceConfiguration` for
  `setDirectorySharingDevices`), use:
  ```rust
  let array: Retained<NSArray<Super>> = NSArray::from_retained_slice(&[sub.into_super()]);
  ```
- `Retained::into_super()` walks the `ClassType::Super` chain
  and is the official way to coerce a `Retained` up the class
  hierarchy.

### Passing `&RcBlock<...>` to a function expecting `&DynBlock<...>`

- The framework API expects `&block2::DynBlock<dyn Fn(...)>`.
- `RcBlock<F>` is `Deref<Target = DynBlock<F>>`, so passing
  `&block` where `&DynBlock<dyn Fn(...)>` is expected
  auto-derefs. No explicit conversion needed.

---

## OCI kernel images (Apple Virtualization guest) — gotchas hit

### Image layout: `/boot/vmlinux` + optional `/boot/initramfs.cpio.gz`

- A nimbus kernel image is a normal OCI image whose layer
  tarball(s) contain:
  ```
  /boot/vmlinux                # required — uncompressed ELF
  /boot/initramfs.cpio.gz      # optional — initramfs
  /usr/lib/nimbus/nimbus-runtime  # optional, future
  ```
- `StagedKernel::from_image` reads these two well-known paths
  (`KERNEL_VMLINUX_PATH = "boot/vmlinux"` and
  `KERNEL_INITRAMFS_PATH = "boot/initramfs.cpio.gz"` in
  `runtime/nimbus-vm/src/oci_kernel.rs`). If `/boot/vmlinux`
  is missing, the error is `OciKernelError::MissingFile`
  with a message asking whether this is a nimbus kernel
  image.
- The kernel MUST be an **uncompressed ELF**, not a bzImage
  or zImage. `VZLinuxBootLoader::initWithKernelURL:` rejects
  compressed images silently (the VM just won't boot).
  `tools/build-kernel-image/build.sh` does
  `aarch64-linux-gnu-strip -s ... arch/arm64/boot/Image -o
  /boot/vmlinux` to produce the right format.

### Image config labels: `org.nimbus.image.kind=kernel`

- The OCI image config MUST carry
  `org.nimbus.image.kind=kernel` so the policy engine can
  tell kernel images apart from container images. The
  runtime rejects (or falls back to defaults for) images
  that are missing the label.
- The label is also how the
  `apple-virt-smoke --kernel-image` flag distinguishes
  "kernel image to stage" from "container image to run as
  a workload" — they look identical at the OCI level
  otherwise.

### `StagedKernel` ownership: `from_image` owns the temp dir; `from_paths` does not

- `StagedKernel::from_image` returns a kernel that owns a
  `tempfile::TempDir` inside it. The `StagedKernel` (and
  the temp dir) lives as long as the `AppleVirtPool` does
  (pool is `Arc<PoolInner>`, so pool drop = kernel drop =
  temp dir cleanup).
- `StagedKernel::from_paths` is the test/caller-staged
  variant: the caller owns the underlying directory; the
  `StagedKernel` does NOT clean it up. Don't use
  `from_paths` in production code that pulls from a
  registry — that's what `from_image` is for.
- Both have the same return type, so
  `AppleVirtPoolConfig::new` doesn't care which was used.

### `OciMaterializer::materialize_bundle` writes the full rootfs

- For a kernel image (~50-100 MB), materializing the whole
  rootfs into a temp dir is fine. For container images
  (hundreds of MB to many GB), this is a v1 problem —
  the materializer would benefit from a "extract just
  these paths" mode. v0 doesn't ship that; the kernel
  image is small enough that it doesn't matter.

### Don't put the kernel image and the workload image in the same MmapStore without namespacing

- The `MmapStore` is content-addressed. Two different
  OCI images with the same layer digest would collide.
  In practice this almost never happens (layers are
  gzipped tarballs and even a single timestamp change
  changes the digest), but if you're pulling both a
  kernel image and a workload image from the same
  registry into the same store, namespace them in the
  store directory (`store/kernels/...` vs
  `store/workloads/...`).
- `apple-virt-smoke` does this implicitly by putting the
  OCI store at `$TMPDIR/apple-virt-smoke-store/oci-store`,
  separate from any runtime's main store.

### Pool takes `StagedKernel` by value, not by reference

- `AppleVirtPoolConfig::new(kernel: StagedKernel, ...)`
  takes the kernel by value. The pool becomes the sole
  owner; the caller's `StagedKernel` is moved into it.
- This avoids a footgun: if the pool held `&StagedKernel`,
  the caller could drop the temp dir before the pool
  shut down, and the framework would try to read a
  vanished `/boot/vmlinux`. By-value ownership makes the
  lifetime explicit: the kernel lives as long as the
  pool.

### Builder methods on `AppleVirtPoolConfig`

- After the refactor, `AppleVirtPoolConfig::new(kernel,
  host_store_path)` only takes the two required fields.
  Pool size, vCPU count, and memory each have a
  `with_*` builder method:
  ```rust
  AppleVirtPoolConfig::new(kernel, store)
      .with_pool_size(3)
      .with_cpus(2)
      .with_mem_mib(1024)
  ```
- The struct fields are still `pub` (for direct
  construction in tests), but production code should
  prefer the builder.


---

## nimbus-vsock protocol

### Frame type discriminants are public

- `nimbus_vsock::FrameType::from_u8(byte)` is `pub` so
  guests and hosts can decode a type tag they read from
  the wire. It is the inverse of `Frame::frame_type()`.
- All 8 frame types are in the 0x01..=0x08 range; values
  outside that range are rejected with
  `ProtocolError::Truncated`.

### `MAX_PAYLOAD` (16 MiB) is a hard cap, not a hint

- `read_frame` and `decode` both check
  `len > MAX_PAYLOAD` and return
  `ProtocolError::PayloadTooLarge { size }` before
  allocating.
- Real workload I/O is fragmented by the writer into
  8–64 KiB chunks (see `nimbus_init::vsock_client`'s
  8192-byte read buffer). 16 MiB is the hard upper
  bound — do not raise it without thinking about
  allocation amplification under attack.

### `encode` is infallible; `decode` is fallible

- `nimbus_vsock::encode(&Frame) -> Bytes` cannot fail
  because `Bytes` is unbounded. The function signature
  is `pub fn encode(&Frame) -> Bytes` (no `Result`).
- `nimbus_vsock::decode(&[u8], FrameType) -> Result<Frame, ProtocolError>`
  is fallible because the payload may be truncated,
  malformed, or contain invalid UTF-8 in string fields.
- If you find yourself writing `let bytes = encode(&f)?;`
  the `?` is a bug — it should be `let bytes = encode(&f);`.

---

## nimbus-init (guest PID 1)

### ⚠️ `tokio::process::Command::current_dir("")` (empty string) makes `spawn()` return `ENOENT` for an otherwise-valid command

- **Severity: HIGH — this caused the longest single debugging session in
  the whole end-to-end VM attach effort (~2 hours of false positives).**
- `nimbus-init` accepts a `WorkloadSpec { working_dir: String, ... }`.
  When the host doesn't specify a working dir, the spec's
  `working_dir` is the empty string `""`.
- The natural code is:
  ```rust
  let mut cmd = Command::new(command);
  cmd.current_dir(&self.working_dir);  // <-- passes "" if unset
  cmd.spawn()                            // <-- returns ENOENT for /bin/sh
  ```
- **Symptom:** the guest logs
  `workload exec failed: spawn /bin/sh: No such file or directory (os error 2)`,
  even though `stat /bin/sh` from the same guest shell shows the
  file exists, has the right size, mode 0755, and is a regular
  executable. The initramfs is correct. The symlink is correct.
  `nimbus-init` just can't `exec()` it.
- **Root cause:** `current_dir("")` is interpreted as a relative
  path of length zero. The kernel resolves this against the
  current dir (probably `/`), and somewhere in the chdir/exec
  pipeline this confuses `execve(2)` into returning `ENOENT`
  for the *binary*, not for the cwd.
- **The fix is to only call `current_dir` when the string is
  non-empty:**
  ```rust
  if !self.working_dir.is_empty() {
      cmd.current_dir(&self.working_dir);
  }
  ```
- This is a one-line fix that cost us hours of investigating the
  WRONG thing: we tried (in order):
  1. Re-checking the initramfs cpio format (it was fine).
  2. Re-checking the busybox symlinks (they were fine, both
     relative and absolute targets).
  3. Re-checking the cpio entry sizes and inode numbers.
  4. Looking at whether the kernel was using a different rootfs
     (it wasn't).
  5. Adding a virtio-console and reading dmesg to confirm the
     kernel mounts the initramfs correctly (it does).
  6. Trying `/bin/busybox` directly as the command (also
     "No such file or directory").
  7. Inspecting busybox's own stat output — it shows the file is
     present, 1.1 MB, mode 0755.
- Eventually we added more `info!()` logging to nimbus-init and
  noticed the spec was being received correctly, the spec's
  `working_dir` was `""`, and removing the `current_dir` call
  made the spawn succeed immediately.
- **Generalization:** any time you see `spawn <command>: No
  such file or directory` in a context where you've confirmed
  the file exists, check whether you're passing a bogus
  `current_dir`. Empty string, "." (which is OK), a path
  containing NUL bytes (which is not OK), or a relative path
  that doesn't exist (which is also not OK — though that
  should be `ENOENT` for the *cwd*, not the *binary*) can all
  trigger this.

### `AF_VSOCK` is Linux-only

- The `connect_vsock` private fn is `#[cfg(target_os = "linux")]`.
- On macOS, the only path is the Unix-domain-socket
  fallback (`$NIMBUS_VSOCK_FALLBACK_UNIX`,
  default `/tmp/nimbus-init.sock`).
- The `VsockClient::connect` API returns
  `VsockError::VsockConnect(io::Error)` on Linux if
  the kernel module isn't loaded or the host isn't
  listening, and `VsockError::UnixConnect` if the
  fallback path is unreachable.

### Vsock fd is `RawFd`; `UnixStream` needs a dup

- The vsock `OwnedFd` is Linux-only and is NOT
  convertible to a `tokio::net::UnixStream` (different
  socket families).
- For per-task I/O we dup the fd with `libc::dup()`,
  wrap in a `std::os::unix::net::UnixStream` (sync),
  set non-blocking, then convert to
  `tokio::net::UnixStream` via `from_std`.
- The helper is `VsockClient::dup_to_tokio_unix(fd)`.

### Tokio rejects blocking sockets in from_std

- `UnixStream::from_std` (and
  `tokio::net::TcpStream::from_std`) check at runtime
  that the socket is registered as non-blocking. If it
  isn't, the call panics with
  "Registering a blocking socket with the tokio
   runtime is unsupported. If you wish to do anyways,
   please add `--cfg tokio_allow_from_blocking_fd` to
   your RUSTFLAGS."
- We hit this in tests when the dup'd fd inherited the
  blocking flag from the parent. The fix: add
  `.cargo/config.toml` with
  `rustflags = ["--cfg", "tokio_allow_from_blocking_fd"]`
  in the workspace root, AND call
  `std_stream.set_nonblocking(false).ok()` before
  `from_std` (the `ok()` is a workaround for some
  platforms where this fails on already-non-blocking
  sockets).
- See tokio-rs/tokio#7172 for the upstream issue.

### WorkloadStdin frame takes `Bytes`, not `[u8; N]`

- The frame enum has `Frame::WorkloadStdin(Bytes)`.
  Constructing one with a fixed-size array fails:
  `Frame::WorkloadStdin(b"hello".into())` — the
  `into()` is `From<&[u8; 5]>` which is NOT
  implemented for `Bytes`.
- Use `bytes::Bytes::from_static(b"...")` for static
  literals, or `Bytes::copy_from_slice(&v)` for owned
  data.

### WorkloadSpec.working_dir is `String`, not `PathBuf`

- We picked `String` to match the wire format
  (length-prefixed) and to keep the on-wire size small.
- Inside nimbus-init, we just pass it to
  `tokio::process::Command::current_dir(&str)`, which
  accepts `impl AsRef<Path>`.

### WorkloadExit uses `exit_code` not `code`

- The wire field is `exit_code: Option<i32>`. Don't
  write `Frame::WorkloadExit { code, signal }` — it's
  `Frame::WorkloadExit { exit_code, signal }`.
- Similarly, the gRPC `AttachExit` has
  `exit_code: int32` + `has_exit_code: bool` +
  `signal: int32` + `has_signal: bool`. The
  `has_*` booleans are protobuf's way of representing
  `Option<i32>`; the `int32` defaults to 0 when the
  `has_*` flag is false.

### Tests must be `#[serial]` (or run in isolation)

- Both nimbus-init tests set
  `NIMBUS_VSOCK_FALLBACK_UNIX` to a tempdir path.
  Concurrent tests would race on the env var and
  potentially try to bind the same path.
- Mark with `#[serial_test::serial]`. The
  `serial_test = "3"` dev-dep is in
  `runtime/nimbus-init/Cargo.toml`.

### Binary will be statically linked

- When we ship nimbus-init into the initramfs, it must
  be built with:
  ```bash
  cargo build -p nimbus-init \
      --target aarch64-unknown-linux-musl \
      --release
  ```
- The crate's `[profile.release]` already has
  `panic = "abort"` + `lto = true` + `opt-level = "z"`
  + `strip = true` to keep the binary small (<500 KiB
  is the target).
- musl is REQUIRED: glibc is not available in the
  initramfs environment.

---

## OCI pull on macOS — reqwest gzip gotcha (FIXED)

### Symptom (pre-fix)

- `nimbusctl pull alpine:3.18` on macOS failed with
  ```
  pull alpine:3.18: rpc error: code = Internal
  desc = pull failed: HTTP error: error decoding
  response body
  ```

### Root cause

- Docker Hub's CDN double-encodes gzip when reqwest's
  `.gzip(true)` decoder is active. Reqwest decompresses
  once, but the result is still gzip-compressed (CDN
  applies Content-Encoding: gzip on top of the manifest's
  own gzip), producing "error decoding response body".

### Fix (applied)

- Removed `.gzip(true)` from the reqwest client builder.
- Set `Accept-Encoding: identity` as a default header
  to suppress server-side gzip encoding.
- Added a `decode_body()` helper that checks the
  response `Content-Encoding` header and manually
  decompresses with `flate2::GzDecoder` if needed.
- Applied to all three body-read paths: manifest,
  config, and layer blobs.

- `OciPuller::get_token` constructs the auth URL as
  `https://{registry}/token?scope=...`. This works
  for Docker Hub (where it's overridden to
  `auth.docker.io`) but is a guess for other
  registries.
- v0 leaves proper `WWW-Authenticate` header parsing
  for the next iteration; the v0 path works for
  `docker.io`, `ghcr.io`, `quay.io`, and
  `registry.gitlab.com`.

---

## build-initramfs

### Hand-rolled cpio, not shelling out

- We generate the newc format in Rust instead of
  shelling out to the host's `cpio` binary. This keeps
  the build hermetic (no macOS BSD cpio / Linux GNU
  cpio differences) and lets us run unit tests
  in-tree.
- The newc header is 110 bytes: 6-byte magic + 13
  8-byte hex fields. Name follows (NUL-terminated),
  padded to 4-byte alignment, then data (also
  4-byte-aligned).
- The kernel recognizes "TRAILER!!!" as the end
  marker; we emit it as a zero-length regular file.

### macOS BSD cpio doesn't extract absolute paths

- `cpio -id < initramfs.cpio` on macOS prints
  "Path is absolute" and skips the file. This is
  because macOS cpio treats the archive as a tar-like
  collection of files relative to the cwd.
- The LINUX KERNEL, when using the initramfs as the
  rootfs, DOES handle absolute paths: it mounts the
  archive at `/` and all the `/init`, `/sbin/...`,
  `/bin/...` paths become the rootfs tree.
- To verify the cpio from macOS, use
  `gunzip -c initramfs.cpio.gz | cpio -t` (list
  only) — this works because listing doesn't care
  about absolute vs relative.

### Don't waste time debugging cpio symlink formats when the real issue is elsewhere

- We burned ~30 minutes once convinced that the issue was the
  cpio symlink format — that the `S_IFLNK` mode was being
  mis-encoded, or that the symlink target string was getting
  corrupted, or that we needed to use a relative vs. absolute
  path for the target.
- The actual issue was in `nimbus-init`'s `Command::current_dir("")`
  call (see the nimbus-init section above). The symlinks were
  always correct, the cpio was always correctly formatted, and
  the kernel was always correctly extracting them. We confirmed
  this with `stat` from inside the guest:
  ```
  $ busybox stat /sbin/nimbus-init
    File: /sbin/nimbus-init
    Size: 2568296   Blocks: 5024   IO Block: 4096   regular file
  Device: 2h/2d   Inode: 14   Links: 1
  Access: (0755/-rwxr-xr-x)
  ```
  And `ls -la /bin/` showed all the symlinks resolving to
  `busybox` with mode 777, 7-byte targets, etc.
- The lesson: when the symptoms don't match the diagnosis
  (the file is clearly there, why does exec say it's not?),
  add a virtio console to the VM and read the kernel+init
  logs *before* spending more time on the file format.

### Why busybox?

- The workload's `nimbus-init` is statically linked
  and doesn't need busybox. But:
  - The kernel mounts `/proc` and `/sys` before
    `exec`'ing `/init`; those mount points need to
    exist as empty directories in the archive.
  - Workload commands inside the VM may want a basic
    POSIX environment (`sh`, `cat`, `mount`, etc.).
  - busybox-static is one binary that provides all
    of these via symlinks.
- We emit symlinks for `cat`, `sh`, `mount`, `umount`,
  `ls`, `echo`, `env`, `true`, `false`, `mkdir`, `rm`,
  `ln`, `cp`, `mv`, `ps`, `sleep`, `test` — all
  pointing at `/bin/busybox`.

### `Cargo.toml` has `[workspace]`

- `tools/build-initramfs/Cargo.toml` declares
  `[workspace]` (empty) so it's its own sub-workspace
  and doesn't pull in the parent nimbus workspace
  deps. This is the standard pattern for standalone
  tools; see `tools/apple-virt-smoke/Cargo.toml` and
  `tools/build-kernel-image/build.sh` for the same
  pattern.

---

## nimbusctl workload run

### Generated proto types are pointer-wrapped oneofs

- The `AttachMessage.Body` oneof has variants like
  `AttachMessage_Stdin{Stdin: *AttachStdin}` — the
  inner field is a POINTER, not a value.
- Construction:
  ```go
  &runtimepb.AttachMessage{
      Body: &runtimepb.AttachMessage_Stdin{
          Stdin: &runtimepb.AttachStdin{Data: chunk},
      },
  }
  ```
- Reading:
  ```go
  case *runtimepb.AttachMessage_Stdout:
      if body.Stdout != nil {
          os.Stdout.Write(body.Stdout.Data)
      }
  ```
- Forgetting the `&AttachStdin{...}` wrapper is a
  compile error: `cannot use chunk (variable of type
  []byte) as *AttachStdin value`.

### `AttachExit` has `HasExitCode` / `HasSignal` booleans

- proto3's int32 cannot represent `None` natively.
  We use a separate `has_exit_code: bool` field; the
  Go getter is `body.Exit.HasExitCode` (NOT
  `body.Exit.GetHasExitCode()` — that's the proto
  compiler-generated one which returns bool
  unconditionally; we want the field value, not a
  synthesized bool).

### SIGINT during attach detaches, doesn't kill the workload

- `nimbusctl workload run` installs a SIGINT handler
  that cancels the context, which closes the gRPC
  stream. The runtime service then sends `StdinEof`
  to the workload's stdin pipe and waits for natural
  exit.
- This is "detach" behavior, not "stop" behavior.
  Use `nimbusctl stop <id>` to forcefully kill the
  workload.
- v0 doesn't have a `nimbusctl workload kill`
  command; use `stop`.

### Bidi streams need BOTH goroutines to return

- If you only close the receive side and the
  workload is still running, the stream stays open
  and the runtime keeps paying for the workload's
  stdout/stderr/exit frames.
- The current code waits for `stdinDone` to close
  before returning, which ensures the send-side
  goroutine has fully drained before we exit.

### `tty` flag is accepted but ignored

- v0 doesn't allocate a pseudo-TTY inside the
  workload. Workloads that need a TTY (interactive
  shells, `vim`, etc.) will see their stdin as a
  pipe, not a TTY, and may behave differently.
- The flag is accepted for forward compatibility —
  when the v1 attach path is wired up, this becomes
  a real `tty: true` option that asks the runtime
  to allocate a pty.

---

## Phase 7: Codebase audit findings

### `brctl` is NOT required

- The code uses `ip link add type bridge` (modern iproute2), not the legacy
  `brctl` from `bridge-utils`. Only `ip` (iproute2) and `iptables` are needed.
- `bridge-utils` was listed in older next-steps checklists but is not a
  runtime dependency. Remove it from server install instructions.

### Root required for loop device mount

- `materialize_ext4_rootfs()` in `runtime/nimbus-vm/src/ext4.rs` calls
  `Command::new("mount")` with `-o loop` to mount the ext4 image before
  populating it via `OciMaterializer::materialize_into()`.
- This requires either root or `CAP_SYS_ADMIN`. The runtime daemon must run
  as root (or with appropriate capabilities).
- No workaround planned for v0 (we need `mount -o loop` for ext4 image
  creation; `e2tools` was considered but deferred).

### Double materialization for Firecracker

- `runtime/nimbus-runtime/src/service.rs:run_workload()` unconditionally
  calls `materialize_rootfs()` (plain directory → `rootfs_cache`) BEFORE
  calling `executor.create()`.
- For Firecracker VMs, `FirecrackerExecutor::create()` then re-materializes
  the same DAG root as an ext4 image via `materialize_ext4_rootfs()`.
- This means: **2x DAG traversal, 2x disk space** (temp dir + ext4 image).
  The plain-dir materialization and its `rootfs_cache` entry are completely
  unused for Firecracker.
- Fix: skip the plain-dir materialization in `run_workload()` when the
  backend is `FirecrackerExecutor`. See `service.rs` around line 1042-1124.

### Missing `/init` wrapper for OCI images booted as VMs

- Firecracker boot args pass `init=/init`. The rootfs produced by
  `materialize_ext4_rootfs()` contains the OCI image's files verbatim.
- OCI container images (e.g. `alpine:latest`) have `ENTRYPOINT`/`CMD` but
  no `/init` executable. The kernel would panic when it cannot find `/init`.
- The `build-initramfs` tool creates a proper `/init` that execs
  `/sbin/nimbus-init`, but that's only for the initramfs — the runtime's
  `materialize_ext4_rootfs()` path doesn't inject any `/init`.
- Fix: inject a `/init` shim script (or nimbus-init binary) into the ext4
  rootfs that reads the image's ENTRYPOINT/CMD from the DAG manifest and
  execs it.

### Firecracker has no OCI-based kernel path

- `oci_kernel.rs` in `nimbus-vm` provides `StagedKernel::from_image()`:
  pulls an OCI kernel image, extracts `/boot/vmlinux`, stores in
  `MmapStore`. This is **only used by the Apple Virt path**.
- Firecracker still requires `--vm-kernel` pointing to a pre-downloaded
  vmlinux file on the host filesystem.
- Fix: call `StagedKernel::from_image()` in the Firecracker code path too,
  or at minimum document that `--vm-kernel` is required.

