# Pullrun Python SDK

Thin synchronous client for the Pullrun runtime gRPC API. It does not spawn
the daemon.

```bash
pullrun-runtime daemon --socket /tmp/pullrun.sock &
pip install -e sdk/python
python examples/sandbox.py
```

```python
from pullrun import Sandbox

with Sandbox("/tmp/pullrun.sock", "alpine:3.18", backend="container",
             command=["sh", "-c", "echo hello"]) as sb:
    print(sb.exec(["cat", "/etc/hostname"]).stdout.decode())
    snap = sb.snapshot()
```
