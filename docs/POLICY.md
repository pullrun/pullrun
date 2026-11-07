# Policy

How Pullrun decides whether an image is allowed to be pulled
and run. The policy engine is the security boundary; the rest
of Pullrun is plumbing.

## The default policy: deny-by-explicit-allow

The engine starts with **everything denied** and adds
allow-rules per the configuration. An unconfigured engine
accepts everything (the runtime runs with `policy: None`).

The motivation: it's much easier to talk about what you want to
*allow* than what you want to *deny*. The OCI ecosystem is
huge; a deny list is doomed to be incomplete. An allow list,
built on cryptographic identities (ed25519 image signatures) and structural
properties (SBOM contents), is auditable.

## What gets evaluated

The policy runs at two points:

1. **At `pull_image`.** Once the image is in the store, the
   engine is asked "is this image OK to be a runnable
   artifact?" A `deny` here returns `permission_denied` to the
   client; the image bytes stay on disk but are not
   registered as runnable.

2. **At `run_workload`.** As defense in depth. The policy
   could have been tightened between the pull and the run;
   this check catches that. A `deny` here returns
   `permission_denied` before any container is created.

Both checks are pure functions of the policy configuration
and the image's stored metadata. There is no network call to
a remote policy server in v0.

## Inputs to the engine

The engine reads the image's stored metadata:

- **ed25519 image signature.** A signature is a small rkyv blob
  stored under a digest derived from the image reference:
  `sha256("pullrun.image-signature.v1\n" + image_ref + "\n")`.
  The public key is referenced by a *trusted key* configured at
  runtime startup (`--trusted-key <id>:<base64>`).
  Verification uses `ed25519_dalek`.

  The signed payload is the fixed byte string

  ```
  pullrun-image-v1\n<image_ref>\n<manifest_digest>\n
  ```

  which binds the *image reference* to the *manifest digest*.
  That is what makes the signature non-spoofable: an attacker
  who swapped the manifest would need a signature over the new
  digest, and the verifier compares the digest inside the
  payload with the manifest it is about to run.

  Note this is **not** Sigstore/Cosign: there is no Rekor
  transparency log, no Fulcio keyless signing and no OCI
  referrers. It is ed25519 over a Pullrun-defined payload,
  which is why the docs and CLI call it an *ed25519 image
  signature* rather than "cosign".

- **CycloneDX SBOM.** Stored as a blob with a deterministic
  digest. The engine parses the SBOM and looks at:
  - `components[]` for `licenses[]` (compared against the
    `deny_licenses` list)
  - `vulnerabilities[]` for the highest CVSS score
    (compared against `max_cvss_score`)

- **Manifest digest.** Used as the SBOM lookup key. The
  assumption is that one manifest maps to one SBOM; in
  practice you may need a build pipeline that produces
  both. (A future v1 will support multiple SBOMs per image
  with a "best coverage wins" selection policy.)

## Configuration

The policy is configured at runtime startup. There is no
runtime reload; restart the runtime to pick up a new policy.

```bash
pullrun-runtime daemon \
    --require-signature \
    --require-sbom \
    --max-cvss 7.0 \
    --trusted-key <id>:<base64-public-key> \
    ...
```

A complete example with a `Policy` object:

```rust
let policy = Policy {
    required_signature: true,
    require_sbom: true,
    max_cvss_score: Some(7.0),
    deny_licenses: vec!["AGPL-3.0".to_string(), "SSPL-1.0".to_string()],
    ..Default::default()
};
let engine = PolicyEngine::new(policy).with_trusted_keys(vec![trusted_key]);
```

In v0 the policy is a *single* `Policy` object applied to all
images. v1 will support per-namespace or per-image-ref policy
overrides (think: Kubernetes `PodSecurityPolicy` but for
images).

## The signature check

The signature check is the only one that uses
public-key cryptography. The flow:

```
signing side (CLI, holds the private seed):
    payload  = "pullrun-image-v1\n<image_ref>\n<manifest_digest>\n"
    sig      = ed25519_sign(private_seed, payload)
    RPC AttachSignature(image_ref, manifest_digest, key_id, sig)

runtime (AttachSignature handler):
    payload  = rebuild the same bytes from the request
    reject unless sig verifies against the trusted key `key_id`
    store.put_blob(signature_digest_for(image_ref), rkyv(SignatureBlob))

runtime (verification, at pull and again at run):
    blob = store.get_blob(signature_digest_for(image_ref))?
    parse rkyv -> { key_id, payload, signature }
    require payload == canonical bytes for (image_ref, manifest_digest)
    require ed25519_verify(trusted_key(with key_id), payload, signature)
```

Two deliberate properties:

- **The runtime rebuilds the payload itself.** A client supplies
  only signature bytes, so it cannot get the runtime to store a
  signature over a payload the operator never authorised. The
  CLI does send the payload it signed, but the runtime uses it
  solely to produce a precise error when the two disagree.
- **The blob key is derived from `image_ref` alone.** Verification
  therefore needs only the image reference to find the
  signature; the manifest digest is checked *inside* the
  payload, so a stale signature for the same reference fails
  closed rather than authenticating a different image.

Because the key is derived from the image reference, sign the
**same string** you pass to `pullrun pull` / `pullrun run`
(`alpine:3.18` and `docker.io/library/alpine:3.18` are
different references).

### Producing attestations

```bash
pullrun keygen -o signing-key.json          # prints "<id>:<base64>"
pullrun pull alpine:3.18
pullrun sign alpine:3.18 --manifest <digest> --key signing-key.json
pullrun sbom attach --manifest <digest> --file cyclonedx.json
pullrun attest alpine:3.18 --manifest <digest>
```

The runtime must be started with the matching public key:

```bash
pullrun-runtime daemon --trusted-key <id>:<base64-public-key> --policy policy.yaml
```

### Trusted keys

Trusted keys are passed at runtime startup as `Vec<TrustedKey>`
(the type predates the rename; it holds an ed25519 verifying
key and an id string). Each key is a raw 32-byte ed25519
public key, base64-encoded, with an id. The engine accepts a
signature verified by *any* of the trusted keys whose id
matches the stored blob.

In v0 the keys are a flat list; v1 will support a key
rotation policy (overlap window, retirement) and a per-key
trust scope (e.g. "this key is only trusted for `ghcr.io/`
images").

## The SBOM check

The SBOM is assumed to be CycloneDX 1.5 JSON. The engine
walks the components and vulnerabilities; the check is
*structural* (does the SBOM exist and parse?) and *semantic*
(do the licenses and CVSS scores fall within policy?).

A missing SBOM is treated as a violation when
`require_sbom: true`. A malformed SBOM (can't parse) is
treated as a violation always — the engine won't silently
allow an image whose SBOM we can't read.

The `max_cvss_score` threshold is the *highest acceptable*
score. A `max_cvss_score: 7.0` rejects any vulnerability with
CVSS >= 7.0; vulnerabilities at 6.9 or below are allowed. This
mirrors the standard CVSS severity band (low: 0.1-3.9,
medium: 4.0-6.9, high: 7.0-8.9, critical: 9.0-10.0).

`deny_licenses` is a list of SPDX license identifiers; the
check is exact-match (case-insensitive). The `+` suffix (e.g.
`GPL-3.0+`) is treated as exact-match against the literal
`GPL-3.0+` — for now, the operator must list every variant
they want to deny.

## Policy decisions are observable

Every policy decision (allow or deny) is recorded in two
places:

1. The `policy_decisions` map on the workload state, returned
   by `pullrun inspect`. The key is the policy name
   (`"default"` in v0), the value is `"allow"` or
   `"deny: <reason>"`.

2. The event bus: `PolicyDenied` (with reason, image_ref,
   policy name) and `PolicyAllowed` (with the same minus the
   reason). The event bus is the right place to *watch* for
   denials; the inspect field is the right place to *audit* a
   specific workload.

## What's not in v0

- **Remote policy service.** v0 is fully local. v1 will
  support a sidecar that calls out to an OPA / Cedar / Sigstore
  Policy Controller for richer rules.
- **Per-image overrides.** v0 is a single global policy. v1
  will support a per-namespace policy.
- **Audit log.** v0 records decisions in memory (and surfaces
  them via inspect + events). A durable audit log on disk is
  a v1 item.
- **Key rotation.** v0 trusts the configured keys until
  restart. v1 will support a key registry with retirement
  dates.

## Runtime hardening primitives

Alongside the image-level policy (signatures, SBOM), the runtime
enforces per-workload hardening flags that are evaluated at
`run_workload` time:

### `--seccomp-profile`

Controls the seccomp BPF filter applied to the workload:

| Value | Behavior |
|-------|----------|
| `default` | A curated allowlist of ~50 syscalls (read, write, mmap, openat, etc.). Blocks raw I/O, kernel module loading, user-namespace creation, and other high-risk syscalls. |
| `unconfined` | No seccomp filter. Required for workloads that need cloned children, user-namespace operations, or uncommon syscalls. |
| custom JSON path | A user-supplied seccomp profile as a JSON file. The runtime passes it directly to runc's `--seccomp-policy`; for VMs it is embedded in the kernel command line via `lsm=seccomp`. |

The `default` profile is the recommended production setting. It
blocks ~250 syscalls that are unnecessary for typical container
workloads while allowing the ~50 that POSIX, Linux ABI, and
common runtimes require.

### `--readonly-rootfs`

When set, the workload's rootfs is mounted read-only. Writes that
would modify image content (e.g. apt-get install, pip install)
fail at the filesystem level. Only explicitly mounted `--volume`
paths and `/run/secrets/` are writable.

This is the primary defense against runtime tampering: even if
an attacker gains code execution inside the workload, they
cannot `chmod +s /bin/sh` or modify system libraries.

### `--no-new-privileges`

Sets the `no_new_privs` process attribute on the workload's
initial process. This prevents `setuid` binary escalation,
`capset` with new capabilities, and `LSM`-based privilege gains.
It is inherited by all child processes and cannot be unset.

In container backends, this maps directly to runc's
`"noNewPrivileges": true`. In VM backends, it is set via the
kernel's `PR_SET_NO_NEW_PRIVS` from the guest agent before the
workload command executes.

### Composition with image policy

The four controls compose orthogonally:

```bash
pullrun-runtime daemon \
  --require-signature \
  --require-sbom \
  --max-cvss 7.0 \
  --readonly-rootfs \
  --no-new-privileges
```

A workload that passes the signature + SBOM checks but has
`--readonly-rootfs` will still fail if it tries to write to
`/usr/`. The policy engine gates *can it run?*; these flags gate
*how it runs* — defense in depth at both layers.

## What this is good for

The engine is *not* a complete supply-chain security
solution. It's the local gate that catches the common
mistakes:

- "I forgot to sign this image." → denied at pull.
- "I built this image with a GPL dependency and didn't notice."
  → denied at pull (if `deny_licenses` lists GPL).
- "I built this image last week and it had a critical CVE."
  → denied at pull (if `max_cvss_score` is set).
- "I tightened the policy last Tuesday but my CI is still
  building the old way." → denied at run (defense in depth).

What it *doesn't* do: stop an attacker who has compromised
the signing key, stop a malicious image that passed
signing+SBOM but has a backdoor in the application code, or
enforce a fine-grained per-deployment policy. Those are all
v1+ work, and they sit on top of this engine — they don't
replace it.
