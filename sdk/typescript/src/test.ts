import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import { RuntimeClient } from "./index.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const PROTO = path.join(here, "..", "proto", "runtime.proto");

function service() {
  const def = protoLoader.loadSync(PROTO, { keepCase: true, longs: String, enums: String, defaults: true });
  return (grpc.loadPackageDefinition(def) as any).pullrun.runtime.Runtime.service;
}

test("run workload against a mock runtime", async () => {
  const server = new grpc.Server();
  server.addService(service(), {
    RunWorkload: (call: any, cb: any) => cb(null, { id: "wl-ts", backend_used: call.request.backend, pid: 1 }),
  });
  const port = await new Promise<number>((resolve, reject) => {
    server.bindAsync("127.0.0.1:0", grpc.ServerCredentials.createInsecure(), (err, p) =>
      err ? reject(err) : resolve(p),
    );
  });
  server.start();
  const client = new RuntimeClient(`127.0.0.1:${port}`);
  try {
    const r: any = await new Promise((resolve, reject) =>
      client.client.RunWorkload({ root_digest: "sha256:" + "0".repeat(64), backend: "container" }, (e: any, v: any) =>
        e ? reject(e) : resolve(v),
      ),
    );
    assert.equal(r.id, "wl-ts");
  } finally {
    client.close();
    server.forceShutdown();
  }
});
