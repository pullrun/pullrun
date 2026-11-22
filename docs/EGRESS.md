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

The **VM and rootless container** backends reject `--egress-deny` with a
clear error rather than silently allowing egress.

## Verified

- An ignored root test builds a throwaway netns, installs the policy,
  and asserts `policy drop`, the allow rule, and the allowed port.
- The runtime logs `fail-closed egress policy applied` after the chain
  is installed.

## Gaps

- **No credential injection yet.** Sandboxes still hold any secrets
  mounted into them; zero-ambient-credential egress is not implemented.
- The full workload traffic path is not yet verified: on the test host
  rootful container veth setup failed with `Cannot find device`, so the
  allowed/denied HTTP checks were inconclusive. This is a rootful
  container networking bug, separate from the nft policy.
- DNS names are resolved once at start; rotating IPs need a restart.
- No egress audit events yet.
- Host networking and `--net host` cannot be made fail-closed and are
  rejected when `--egress-deny` is set.
