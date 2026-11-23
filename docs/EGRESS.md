# Fail-closed egress

Pullrun can deny workload egress by default and allow only an explicit
outbound allowlist. This is opt-in; without it the runtime keeps the
existing allow-all bridge/NAT behaviour.

## Usage

```bash
# Per workload: deny everything except 1.1.1.1:80 and configured DNS.
pullrun run alpine:3.18 --backend container --net bridge \
  --egress-deny --allow-outbound tcp:1.1.1.1:80 \
  --cmd /bin/sh --cmd -c --cmd 'wget -qO- http://1.1.1.1'

# Or deny by default for every workload on the daemon.
pullrun-runtime daemon --socket ... --egress-deny
```

## How it works

For **rootful containers**, Pullrun enters the container's network
namespace after the veth is up and installs an nftables chain:

```
table inet pullrun_egress {
  chain output {
    type filter hook output priority 0; policy drop;
    oifname "lo" accept
    ct state established,related accept
    ip daddr <dns-server> udp/tcp dport 53 accept
    ip daddr <allowed-ip> tcp dport <port> accept
  }
}
```

Hostnames in `--allow-outbound` are resolved on the host at start time
and their IPv4 addresses are installed as rules. A failure to install
the policy aborts workload startup (fail closed).

**Bridge-networked Firecracker VMs** enforce `--egress-deny` with a
per-VM nftables forward/input policy (DNS + allowlist + proxy accepted,
everything else dropped). Slirp VMs and rootless containers still reject
the flag; use bridge networking for fail-closed VM egress.

VM networking is therefore safe by default: `pullrun run --backend vm`
starts the VM **isolated**. Pass `--net slirp` (or `--trusted`) to allow
egress explicitly. MCP's `run` tool behaves the same way (`trusted:
true` opts in).

## Credential injection (HTTPS MITM)

Start the daemon with one or more credential rules. The rule format is
`HOST:HEADER=SECRET`; the secret is read from Pullrun's encrypted secret
store and never mounted into the sandbox.

```bash
printf 'Bearer sk-...' | pullrun secret create OPENAI_KEY -
pullrun-runtime daemon --socket /tmp/pullrun.sock   --egress-credential api.openai.com:authorization=OPENAI_KEY
```

Credential rules imply `--egress-deny`. Workloads receive:

- `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` pointing at the proxy,
- a read-only CA certificate at `/etc/pullrun-ca.pem`,
- `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`,
  `NODE_EXTRA_CA_CERTS` pointing at it.

The proxy terminates CONNECT TLS with a per-host certificate signed by a
per-daemon CA, injects the configured header, and re-originates TLS to
the real host. Only hosts with a credential rule are allowed. Connections
from non-private source addresses are rejected before parsing.

Verified on the KVM host with `curlimages/curl`:

```
Authorization: Bearer test123
```

with a container that had no `/run/secrets/TEST_SECRET`
(`no_secret_file=0`) and no `TEST_SECRET` environment variable
(`no_secret_env=0`). The host-side proxy test also returned 200.

## Verified

- An ignored root test builds a throwaway netns, installs the policy,
  and asserts `policy drop`, the allow rule, and the allowed port.
- **Traffic test on the KVM host (rootful container):**
  - `nc 1.1.1.1:80` (allowed) → **exit 0**
  - `nc 93.184.216.34:80` (not allowed) → **exit 1**
  - Both runs used `--egress-deny --allow-outbound tcp:1.1.1.1:80`.
- Rootful container networking was also fixed: the bridge is brought
  up, the default `10.42.0.0/16` gets MASQUERADE, and FORWARD accepts
  traffic to/from the bridge (Docker sets FORWARD to DROP).

## Gaps

- **Explicit `--secret` mounts are still supported** (docker-style);
  they place credentials inside the sandbox. Use egress credentials
  instead for zero-ambient-credential workloads.
- The MITM proxy handles HTTP/1.1 CONNECT requests; HTTP/2-only
  upstreams and non-TLS protocols are not covered.
- The CA private key lives in the daemon process; the public CA cert is
  written to `<store>/egress/ca.pem`. `pullrun network rotate-ca` rotates
  it; sandboxes must restart to trust the new CA.
- DNS names are resolved once at start; rotating IPs need a restart.
- Rootful containers and bridge-networked VMs are enforced; slirp VMs
  and rootless containers reject `--egress-deny`.
- VM egress emits a rate-limited kernel audit line per denied flow:
  `journalctl -k | grep "pullrun-audit <workload-id>"` (SRC/DST/DPT,
  rate 10/s). There is no structured EventBus audit stream yet.
- CA rotation: `pullrun network rotate-ca`; sandboxes must restart.
- No egress audit events yet.
- Host networking and `--net host` cannot be made fail-closed and are
  rejected when `--egress-deny` is set.
