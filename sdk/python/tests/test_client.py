import unittest
from concurrent import futures

import grpc

from pullrun import RuntimeClient
from pullrun import runtime_pb2 as pb
from pullrun import runtime_pb2_grpc as pbg


class _Service(pbg.RuntimeServicer):
    def ListWorkloads(self, request, context):
        return pb.ListWorkloadsResponse()

    def RunWorkload(self, request, context):
        return pb.RunResponse(id="wl-test", backend_used=request.backend, pid=1)


class SDKTest(unittest.TestCase):
    def test_run_and_list(self):
        server = grpc.server(futures.ThreadPoolExecutor(max_workers=2))
        pbg.add_RuntimeServicer_to_server(_Service(), server)
        port = server.add_insecure_port("127.0.0.1:0")
        server.start()
        client = RuntimeClient(f"127.0.0.1:{port}")
        try:
            run = client.stub.RunWorkload(
                pb.RunRequest(root_digest="sha256:" + "0" * 64, backend="container")
            )
            self.assertEqual(run.id, "wl-test")
            self.assertEqual(len(client.stub.ListWorkloads(pb.ListWorkloadsRequest()).workloads), 0)
        finally:
            client.close()
            server.stop(0)


if __name__ == "__main__":
    unittest.main()
