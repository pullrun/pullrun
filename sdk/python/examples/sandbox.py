from pullrun import Sandbox

with Sandbox("/tmp/pullrun.sock", "alpine:3.18", backend="container",
             command=["sh", "-c", "echo hello"]) as sb:
    out = sb.exec(["echo", "sdk-ok"])
    print(out.stdout.decode().strip(), out.exit_code)
