# Actors and golden templates

An actor is a stateful workload that can be suspended to a snapshot and
resumed on demand. Actors start from a **golden template**: a memory
snapshot of a prepared workload. This is Pullrun's single-node take on
Agent Substrate's actor model — no Kubernetes required.

## Workflow

```bash
# 1. Prepare a workload (install deps, warm caches, start an agent loop).
pullrun run alpine:3.18 --backend vm --net none \
  --cmd /bin/sh --cmd -c --cmd 'while true; do date; sleep 1; done'
# -> workload wl-...

# 2. Snapshot it into a named golden template. This stops the workload.
pullrun template create my-agent --from-workload wl-...

# 3. Instantiate a suspended actor from the template.
pullrun actor create agent-1 --template my-agent
pullrun actor ls
# ACTOR    TEMPLATE  STATE      WORKLOAD
# agent-1  my-agent  suspended

# 4. Resume on demand, then park it again.
pullrun actor resume agent-1      # -> running, ~100 ms
pullrun actor suspend agent-1     # -> snapshot, stopped

# 5. Resume + attach to the guest console (a request).
pullrun actor run agent-1
```

## Request parking

Resume and suspend take a per-actor mutex. Concurrent requests for the
same actor wait for the in-flight transition instead of racing or
failing; once the actor is running, later `resume` calls return the
existing workload immediately (`resumed=false`).

## Measured on the KVM host

| step | result |
|---|---|
| template create (memory snapshot) | snapshot captured, source stopped |
| actor resume, first | **110 ms** |
| actor suspend (memory snapshot) | snapshot written |
| actor resume, second | **105 ms** |

## Storage

```
<store>/actors/
  templates/<name>.json   # snapshot id, image, command, timestamp
  instances/<name>.json   # template, state, snapshot id, workload id
<store>/snapshots/<id>/   # state, mem, rootfs.ext4, meta.json
```

## Limitations

- Actors are **single-node**; there is no cluster scheduler or remote
  worker pool.
- `actor run` parks on the console stream; there is no HTTP ingress
  router or request queue yet.
- Suspending keeps the memory snapshot, so each suspended actor costs
  its guest RAM on disk (512 MiB in tests). There is no eviction or
  per-actor quota.
- Templates are immutable snapshots; updating a template requires
  creating a new one.
- Firecracker/Linux only for memory fidelity.
