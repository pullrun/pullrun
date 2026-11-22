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
- CRI mount propagation maps to rprivate/rslave/rshared; ContainerStatus
  returns the request mounts. rshared/rslave propagation still fails.
- Privileged containers now get the full `CAP_*` capability set instead
  of the invalid literal `ALL`.
- Container labels/annotations/metadata attempt preserved.
- Host networking omits the OCI network namespace (hostNetwork pods
  really share the host netns).
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
| Container (basic operations) | **14/14 pass** |
| Container (full) | **22/31 before mount fixes**; mount specs now 2/4 (rprivate + readonly pass; rshared/rslave fail). Multi-container suite is blocked on slow/failing test-image pulls. Bridge fallback (shared bridge when no per-pod /24) is in but not yet validated. DAG conversion now batches writes and fsyncs once; the speedup is not yet measured |
| Container logs | **2/2 pass** |
| Container stats | **5/5 pass** |
| Image Manager (core) | **11/15 pass**; remaining: registry manifest-digest IDs, all-reference status, Uid/Username from image config, identifier consistency |
| Image volumes | feature-gated, not implemented |
| Stats | **5/5 pass** |
| Streaming | **2/5 pass**; host networking now omits the OCI network namespace (fixes hostNetwork pods), TTY forwarding is wired, but the suite still needs re-verification |

A full 142-spec run is not yet green: an earlier run wedged in
`StopPodSandbox` before the idempotence fix, and the complete suite has
not been re-run end to end. Treat container/image/streaming conformance
as **in progress**, not complete.

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
