# Pullrun Python SDK

Thin synchronous client for the Pullrun runtime gRPC API. It does not spawn
the daemon.

Python's grpc-core currently gets `RST_STREAM` from the daemon's UDS
transport; use the explicit TCP listener until that is fixed:

```bash
pullrun-runtime daemon --socket 127.0.0.1:50561 &
pip install -e sdk/python
python examples/sandbox.py
```

`Sandbox("127.0.0.1:50561", ...)` uses TCP. UDS targets (`unix:///...`)
remain the default for Go/Node clients.

```python
from pullrun import Sandbox

with Sandbox("/tmp/pullrun.sock", "alpine:3.18", backend="container",
             command=["sh", "-c", "echo hello"]) as sb:
    print(sb.exec(["cat", "/etc/hostname"]).stdout.decode())
    snap = sb.snapshot()
```
