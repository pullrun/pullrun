# Kubernetes CRI shim

`pullrun-cri` maps the Kubernetes Container Runtime Interface (CRI v1)
onto the Pullrun runtime. It is now built against **Kubernetes 1.37**
(`k8s.io/cri-api v0.37.2`).

## Runtime classes

| RuntimeClass | Backend |
|---|---|
| `pullrun-container` | runc container (trusted tier) |
| `pullrun-vm` | Firecracker / Apple Virtualization VM |

Pods get a per-pod `/24` bridge (`pr-<pod-id>`); containers join the
sandbox network namespace with `network_mode=container:<sandbox-id>`.

## Implemented CRI surface

- Runtime: Version, Status, Run/Stop/Remove/List/Status PodSandbox,
  Create/Start/Stop/Remove/List/Status Container, ExecSync, Exec,
  Attach, PortForward, container/pod stats, UpdateRuntimeConfig,
  RuntimeConfig, image file-system info.
- Image: ListImages, ImageStatus, PullImage, RemoveImage.
- Streaming: CRI v4 remotecommand handshake (stdin EOF, exit code on the
  error stream).
- Pod semantics: metadata/attempt, labels, annotations, DNS config,
  hostNetwork, per-pod bridge, Linux sysctls (applied to the sandbox and
  every pod container).
- Idempotent StopPodSandbox/StopContainer for missing workloads.
- CRI Created state: `CreateContainer` creates without starting
  (`create_only`), `StartContainer` calls the new `StartWorkload` RPC.
- Container stdout/stderr are written to `Config.LogPath` in CRI log
  format (`RFC3339Nano stream tag message`); relative paths are joined
  with the sandbox `LogDirectory`; `ReopenContainerLog` recreates the
  file and the writer follows the new inode. Verified: log tests 2/2.

## Conformance

The shim is tested with `critest` v1.37.0 from `cri-tools`:

```bash
critest -runtime-endpoint unix:///run/pullrun/pullrun-cri.sock \
        -image-endpoint unix:///run/pullrun/pullrun-cri.sock
```

Current status on the Linux KVM host:

| Suite | Result |
|---|---|
| Runtime info (version, conditions) | **2/2 pass** |
| PodSandbox (including attributes, sysctls, idempotence) | **9/9 pass** |
| Container | in progress — log tests 2/2 pass; full suite rerun still in progress |
| Image / Streaming / Stats | not yet run |

A full 142-spec run is not yet green: an earlier run wedged in
`StopPodSandbox` before the idempotence fix, and the complete suite has
not been re-run end to end. Treat container/image/streaming conformance
as **in progress**, not complete.

## Known gaps

- Pod-level cgroup parent and pod overhead/resources are recorded but
  not fully enforced.
- User namespaces, image volumes, and checkpoint/restore CRI features
  are not implemented.
- `ImageStatus` reports the DAG size; multi-arch image list metadata is
  minimal.
