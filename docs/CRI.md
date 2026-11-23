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
- OOM-killed containers report `OOMKilled`.
- CRI mount propagation is applied with open_tree/move_mount into the
  container mount namespace (rprivate/rslave/rshared). ContainerStatus
  returns the request mounts.
- Privileged containers now get the full `CAP_*` capability set instead
  of the invalid literal `ALL`.
- Container labels/annotations/metadata attempt preserved.
- Host networking omits the OCI network namespace (hostNetwork pods
  really share the host netns).
- StartWorkload preserves the sandbox netns join for create-only
  containers (multi-container networking works).
- ContainerStatus exposes LogPath (multi-container logs work).
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
| PodSandbox (attributes, sysctls, idempotence) | **9/9 pass** |
| Container basics | **14/14 pass** |
| Container full | **30/31 pass** (multi-container 3/3, mount propagation 4/4); only image volume remains |
| Networking | **4/4 pass** |
| Conformance subset | **50/50 pass** |
| Container logs | **2/2 pass** |
| Container stats | **5/5 pass** |
| Image core | **15/15 pass** |
| Streaming | **5/5 pass** |
| Image volumes | feature-gated, not implemented |
| OCI conversion | httpd (300 MB) pulls + converts in **~25 s** |

A full 142-spec run is not yet a gate: it hits the 20-minute cap and
includes optional benchmark/event/feature specs. The per-suite results
above are the authoritative conformance evidence. Remaining CRI gap: Kubernetes image volumes (feature-gated).

## Known gaps

- Pod-level cgroup parent and pod overhead/resources are recorded but
  not fully enforced.
- User namespaces, image volumes, and checkpoint/restore CRI features
  are not implemented.
- Image volumes (`Feature:ImageVolume`) are not implemented.
- The puller now computes and persists the registry manifest digest,
  but CRI image IDs intentionally remain the DAG root. Using the
  manifest digest as the CRI ID regressed conformance (11/15 → 7/15),
  so the mapping needs a consistent design before switching.
- Image IDs use the DAG root digest; registry manifest digests are not
  preserved separately.
