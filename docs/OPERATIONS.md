# Operations

A guide for running Nimbus in production: deploying on
Kubernetes, configuring the policy engine, monitoring, and
troubleshooting common failure modes.

## Deployment

### Direct mode (single host, development)

The simplest deployment: run `nimbusctl` on a single Linux host
with `/dev/kvm` (for the VM backend). The CLI spawns the
runtime as a child process over a Unix domain socket; no daemon
to manage.

```bash
# Build
cargo build --release
ls target/release/nimbus-runtime target/release/nimbusctl

# Run
./target/release/nimbusctl pull alpine:3.18
./target/release/nimbusctl run sha256:6a... --backend container
./target/release/nimbusctl list
./target/release/nimbusctl inspect wl-abc123
```

For a long-lived runtime on a single host, run the runtime
daemon directly:

```bash
./target/release/nimbus-runtime daemon \
    --socket /var/run/nimbus.sock \
    --store-root /var/lib/nimbus \
    --metrics-addr 0.0.0.0:9090

# Then point the CLI at it
./target/release/nimbusctl --socket /var/run/nimbus.sock --direct=false list
```

### Kubernetes (DaemonSet, production)

The `deploy/` directory has the manifests you need:

```bash
kubectl apply -f deploy/serviceaccount.yaml
kubectl apply -f deploy/servicemonitor.yaml
kubectl apply -f deploy/prometheusrule.yaml
kubectl apply -f deploy/runtime-daemon.yaml
```

The DaemonSet runs one runtime per node. The ServiceMonitor
tells the Prometheus Operator to scrape `/metrics` on port 9090
every 30 seconds. The PrometheusRule ships five alerts (see
README for the list).

To use the VM backend in Kubernetes, register a RuntimeClass:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nimbus-vm
handler: nimbus-vm
```

Then any pod with `spec.runtimeClassName: nimbus-vm` will be
scheduled on a Nimbus node and executed as a Firecracker
microVM. The CRI shim handles the translation.

### Required host setup

- **`/dev/kvm` readable by the runtime user.** On most distros
  this means the `kvm` group; the DaemonSet's `securityContext`
  sets `privileged: true` for v0 because that's the path of
  least resistance. In v1 we'll narrow this with
  `deviceGroups` / `cgroupAccess`.
- **iptables with the nat table available.** Required for VM
  outbound NAT. Most distros have this; in a locked-down
  container image, you may need `iptables-nft` or
  `iptables-legacy` and a custom kubelet.
- **bridge / tun kernel modules.** Required for the shared
  workload network. Most distros have them; the kernel
  autoloads on first use.
- **Disk space.** A pull of `alpine:3.18` uses ~3 MB; a full
  Ubuntu image with apt cache uses ~700 MB. The store grows
  monotonically; the `NimbusStoreGrowingFast` alert fires
  before it fills the disk.

### Rootless operation

Nimbus can run most VM operations without root:

| Operation | Rootless? | How |
|---|---|---|
| OCI pull → DAG store | ✅ Always | Filesystem writes only |
| ext4 rootfs build | ✅ Always | `mkfs.ext4 -d` (no loop-mount) |
| TAP device creation | ✅ With setcap | `ioctl(TUNSETIFF)` on `/dev/net/tun`; binary needs `setcap cap_net_admin=eip` |
| Bridge creation + `ip link` | ⚠️ Needs `CAP_NET_ADMIN` | `ip link add type bridge` via subprocess |
| iptables NAT rules | ❌ Needs root | `iptables` subprocess |
| Firecracker VM boot | ⚠️ Needs `/dev/kvm` access | kvm group or privileged |

To enable rootless TAP creation:
```bash
sudo setcap cap_net_admin=eip /usr/local/bin/nimbus-runtime
getcap /usr/local/bin/nimbus-runtime
# Expected: /usr/local/bin/nimbus-runtime cap_net_admin=eip
```

## Configuration

### CLI flags (runtime daemon)

| Flag | Default | Purpose |
|---|---|---|
| `--socket` | `/tmp/nimbus.sock` | gRPC UDS path |
| `--store-root` | `/var/lib/nimbus` | Where the DAG lives |
| `--metrics-addr` | (none) | Bind Prometheus `/metrics` HTTP; pass no value to use 127.0.0.1:9090 |
| `--require-signature` | false | Reject unsigned images |
| `--require-sbom` | false | Reject images without a CycloneDX SBOM |
| `--max-cvss <SCORE>` | (none) | Reject images with vulnerabilities above this CVSS |
| `--readonly-rootfs` | false | Declare the rootfs must be read-only |
| `--no-new-privileges` | false | Set `no_new_privs` on the container |
| `--vm-firecracker <path>` | (none) | Path to the `firecracker` binary |
| `--vm-kernel <path>` | (none) | Path to the Linux kernel image for VMs |
| `--vm-root <path>` | (none) | Where VM rootfs blobs are materialized |
| `--vm-vcpus <N>` | 1 | Default vCPUs per VM |
| `--vm-mem <MiB>` | 512 | Default memory per VM |

### Environment variables

- `NIMBUS_STORE` — overrides `--store-root` for the CLI.
  Useful for shared dev environments.

## Monitoring

### Key metrics (Prometheus)

| Metric | Type | Labels | What it tells you |
|---|---|---|---|
| `nimbus_pulls_total` | counter | `registry`, `status` | Pull throughput; ratio of `failed`/`denied` to `success` |
| `nimbus_pull_duration_seconds` | histogram | — | Pull latency distribution (p50/p95/p99) |
| `nimbus_workloads_started_total` | counter | `backend` | How many workloads have started per backend |
| `nimbus_workloads_running` | gauge | `backend` | Live workload count |
| `nimbus_workload_start_duration_seconds` | histogram | — | Workload start latency (create + start) |
| `nimbus_workload_exits_total` | counter | `backend`, `code` | Exit code distribution; `code=137` is SIGKILL (OOM) |
| `nimbus_store_nodes` | gauge | — | Number of DAG nodes currently cached |
| `nimbus_store_bytes` | gauge | — | Total bytes cached |

`status` on pulls has four values: `started`, `success`, `failed`,
`denied`. The `denied` counter is a security signal — a non-zero
rate means a workload tried to run an image that violated policy.

### Alerts (in `deploy/prometheusrule.yaml`)

The shipped alerts target the most common operational
concerns. Tune them to your environment:

- `NimbusRuntimeDown` (2m, critical) — the daemon is not
  scraping. The host is probably down or `/dev/kvm` is gone.
- `NimbusPullFailureRate` (5m, warning) — more than 25% of
  pulls are failing. Often a transient registry issue; check
  the runtime's stderr for HTTP errors.
- `NimbusWorkloadCrashLoop` (10m, warning) — exit codes other
  than 0/137/unknown are firing at > 0.1/s. Something is
  crashing repeatedly; check the logs.
- `NimbusPullLatencyHigh` (10m, warning) — p95 > 30s. Usually
  network-bound; the histogram buckets will tell you whether
  it's tail latency or a uniformly slow pull.
- `NimbusStoreGrowingFast` (30m, info) — store growing > 1
  GB/hour. Warns before the disk fills. v0 has no GC, so this
  is a "plan more disk" signal.

### Grafana dashboard

`deploy/grafana-dashboard.json` is a 6-panel dashboard:

1. Pull rate (per registry, success/failed/denied split)
2. Workloads running (per backend)
3. Pull + start latency p50/p95/p99
4. Exit code distribution
5. Store size + node count
6. Per-node runtime up (`up{job="nimbus-runtime"}`)

Import the JSON; the panel queries assume a Prometheus
scrape job named `nimbus-runtime`.

## Troubleshooting

### "I pulled but `inspect` shows `image_root` is empty"

The workload was never run. `inspect_workload` only knows about
workloads that have been started; the pull alone doesn't create
a workload record. (This is intentional — pulls are cheap and
don't have lifecycle. The image is in the store; just run it.)

### "Pull fails with `error decoding response body`"

The OCI puller has a known bug in v0 when the registry returns
an OCI auth response with an unexpected content type. The
common culprit is a private registry that requires a custom
auth flow. Workaround: pass the credentials via a custom
`Registry` config object (not yet exposed in v0); or pull from
a public registry.

Tracked as a v0.1 known issue.

### "VM boot times out, no `nimbus-vm-outbound OK` in the log"

Three things to check, in order:

1. **`/dev/kvm` readable.** `ls -la /dev/kvm` should show the
   runtime user with `rw` access. If not, add the user to the
   `kvm` group, or `chmod 666 /dev/kvm` (not for production).

2. **The kernel and rootfs paths are correct.** The runtime
   needs the path to a vmlinux-style kernel *and* a directory
   where VM rootfs blobs can be materialized. Both are
   passed as CLI flags.

3. **The bridge is up.** `ip link show nimbus-br0` should show
   the bridge with `state UP`. If it doesn't, the runtime
   will print an iptables error on the next operation.

Use `tools/vm-outbound-smoke/` as a standalone reproducer. It
boots a minimal Alpine VM and runs a single `wget` against a
host-bound HTTP server; a successful run prints
`nimbus-vm-outbound OK` to the guest's serial console.

### "Container starts but I can't reach it on its IP"

The proxy needs a `NetworkRule` with `direction: inbound` for
the port you want exposed. The default
`NetworkMode::Loopback` means no external traffic; the
workload is reachable only from the host (and only via the
proxy on `10.42.0.1`).

For a workload to be reachable from outside the host, add a
listening port:

```bash
nimbusctl run sha256:... \
    --allow-inbound 8080 \
    --cmd my-server
```

The proxy will listen on `10.42.0.1:8080` and forward to the
container's internal IP. To expose the proxy port on the
host's public interface, you need an additional
`iptables -t nat -A PREROUTING -p tcp --dport 8080 -j DNAT
--to-destination 10.42.0.1:8080` (not done automatically in v0;
left to the operator's firewall automation).

### "Prometheus shows `up == 0` for `nimbus-runtime`"

The runtime's HTTP server isn't reachable from Prometheus. In
order of likelihood:

1. The `--metrics-addr` flag was passed but the cluster
   network policy doesn't allow port 9090. Check the
   `NetworkPolicy` in `deploy/runtime-daemon.yaml` — the
   shipped manifest opens 9090 from the cluster CIDR.

2. The runtime crashed on startup. Check the pod's
   `kubectl logs` — the most common cause is `/dev/kvm` not
   being readable.

3. The ServiceMonitor selector doesn't match. The shipped
   manifest uses `release: kube-prometheus-stack`; if your
   cluster uses a different label, the ServiceMonitor won't
   find the pods.

### "Workload exits with code 137"

That's `128 + 9` — the kernel sent SIGKILL. The usual cause
is the OOM killer. Check the host's dmesg (`dmesg | grep -i
killed`) for the actual culprit. To prevent it, raise
`--memory` (CLI flag) or set a memory limit on the pod's
`resources.limits.memory` if running in Kubernetes.

### "The store is huge and growing"

v0 has no garbage collection. The store grows monotonically as
you pull images. To reclaim space, manually delete the store
directory (`rm -rf /var/lib/nimbus`) — the next pull will
rebuild it. v1 will add an LRU eviction policy based on
last-access time of each DAG node.

## Upgrades

v0 doesn't have an upgrade story. Pull a newer release, restart
the runtime, and re-pull any images whose digests changed (in
practice: any image that has a new tag).

The on-disk format is stable as long as the runtime version
is. A mismatch (older runtime reading newer-format files) will
fail with a `check_bytes` error from rkyv; a downgrade is
always safe (the older runtime ignores fields it doesn't
know about).
