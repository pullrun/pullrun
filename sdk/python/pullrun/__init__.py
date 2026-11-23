"""Thin Python SDK for the Pullrun runtime (gRPC over UDS/TCP).

The SDK does not start the daemon. Start it first:

    pullrun-runtime daemon --socket /tmp/pullrun.sock

Then:

    from pullrun import Sandbox
    with Sandbox("/tmp/pullrun.sock", "alpine:3.18", backend="container",
                 command=["sh", "-c", "echo hello"]) as sb:
        print(sb.exec(["cat", "/etc/hostname"]).stdout.decode())
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from typing import Iterable, Iterator, Optional

import grpc

from . import runtime_pb2 as pb
from . import runtime_pb2_grpc as pbg

__all__ = ["RuntimeClient", "Sandbox", "ExecResult", "RestoreResult"]


def _channel(target: str) -> grpc.Channel:
    if target.startswith("unix://") or target.startswith("/"):
        path = target.removeprefix("unix://")
        return grpc.insecure_channel(f"unix://{path}")
    return grpc.insecure_channel(target)


@dataclass
class ExecResult:
    exit_code: int
    stdout: bytes
    stderr: bytes


@dataclass
class RestoreResult:
    workload_id: str
    backend: str
    restore_ms: int


class RuntimeClient:
    def __init__(self, target: str):
        self.channel = _channel(target)
        self.stub = pbg.RuntimeStub(self.channel)

    def close(self) -> None:
        self.channel.close()


class Sandbox:
    """A single workload. Use as a context manager or call start()/stop()."""

    def __init__(
        self,
        target: str,
        image: str,
        *,
        backend: str = "container",
        command: Optional[Iterable[str]] = None,
        env: Optional[dict[str, str]] = None,
        name: Optional[str] = None,
        network: Optional[str] = None,
        cpu_millicores: Optional[int] = None,
        memory_bytes: Optional[int] = None,
        kernel_image: Optional[str] = None,
        registry: str = "",
        platform: str = "",
    ):
        self.client = RuntimeClient(target)
        self.image = image
        self.backend = backend
        self.command = list(command or [])
        self.env = dict(env or {})
        self.name = name or ""
        self.network = network
        self.cpu_millicores = cpu_millicores
        self.memory_bytes = memory_bytes
        self.kernel_image = kernel_image
        self.registry = registry
        self.platform = platform
        self.id: Optional[str] = None

    @property
    def stub(self):
        return self.client.stub

    def start(self) -> "Sandbox":
        req = pb.PullImageRequest(image_ref=self.image, registry=self.registry)
        if self.platform:
            req.platform = self.platform
        pulled = self.stub.PullImage(req)
        name = self.name or f"sdk-{uuid.uuid4().hex[:12]}"
        run = pb.RunRequest(
            id=name,
            root_digest=pulled.root_digest,
            backend=self.backend,
            command=self.command,
            env=self.env,
        )
        if self.network is not None:
            run.network_mode = self.network
        if self.cpu_millicores:
            run.cpu_millicores = self.cpu_millicores
        if self.memory_bytes:
            run.memory_bytes = self.memory_bytes
        if self.kernel_image:
            run.kernel_image = self.kernel_image
        resp = self.stub.RunWorkload(run)
        self.id = resp.id
        return self

    def exec(self, command: Iterable[str], timeout_seconds: int = 0) -> ExecResult:
        if not self.id:
            raise RuntimeError("sandbox not started")
        r = self.stub.ExecInWorkload(
            pb.ExecRequest(id=self.id, command=list(command), timeout_seconds=timeout_seconds)
        )
        return ExecResult(r.exit_code, r.stdout, r.stderr)

    def logs(self, follow: bool = True, tail: int = 100) -> Iterator[bytes]:
        if not self.id:
            raise RuntimeError("sandbox not started")
        stream = self.stub.StreamLogs(pb.StreamLogsRequest(id=self.id, follow=follow, tail=tail))
        for chunk in stream:
            yield chunk.data

    def status(self):
        if not self.id:
            raise RuntimeError("sandbox not started")
        return self.stub.GetWorkload(pb.GetWorkloadRequest(id=self.id))

    def snapshot(self, fidelity: str = "memory"):
        if not self.id:
            raise RuntimeError("sandbox not started")
        return self.stub.CreateSnapshot(pb.CreateSnapshotRequest(workload_id=self.id, fidelity=fidelity))

    def stop(self) -> None:
        if self.id:
            try:
                self.stub.StopWorkload(pb.StopRequest(id=self.id))
            finally:
                self.id = None

    def close(self) -> None:
        self.stop()
        self.client.close()

    def __enter__(self) -> "Sandbox":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.close()


def restore(target: str, snapshot_id: str, *, fork: bool = False) -> RestoreResult:
    client = RuntimeClient(target)
    try:
        r = client.stub.RestoreSnapshot(
            pb.RestoreSnapshotRequest(snapshot_id=snapshot_id, fork=fork)
        )
        return RestoreResult(r.workload_id, r.backend, r.restore_ms)
    finally:
        client.close()
