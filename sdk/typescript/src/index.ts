/**
 * Thin TypeScript SDK for the Pullrun runtime gRPC API.
 *
 * The SDK does not start the daemon. Start it first:
 *   pullrun-runtime daemon --socket /tmp/pullrun.sock
 */
import path from "node:path";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";

const here = path.dirname(fileURLToPath(import.meta.url));
const PROTO = path.join(here, "..", "proto", "runtime.proto");

type Dict = Record<string, unknown>;

function loadProto(): grpc.GrpcObject {
  const def = protoLoader.loadSync(PROTO, {
    keepCase: true,
    longs: String,
    enums: String,
    defaults: true,
    oneofs: true,
  });
  return grpc.loadPackageDefinition(def);
}

function address(target: string): string {
  if (target.startsWith("unix://")) return "unix:" + target.slice("unix://".length);
  if (target.startsWith("/")) return "unix:" + target;
  return target;
}

function unary<T>(client: any, method: string, req: Dict): Promise<T> {
  return new Promise((resolve, reject) => {
    client[method](req, (err: Error | null, res: T) => (err ? reject(err) : resolve(res)));
  });
}

export class RuntimeClient {
  readonly client: any;

  constructor(target: string) {
    const proto = loadProto() as any;
    this.client = new proto.pullrun.runtime.Runtime(address(target), grpc.credentials.createInsecure());
  }

  close(): void {
    this.client.close();
  }
}

export interface ExecResult {
  exitCode: number;
  stdout: Buffer;
  stderr: Buffer;
}

export interface SandboxOptions {
  backend?: string;
  command?: string[];
  env?: Record<string, string>;
  name?: string;
  network?: string;
  cpuMillicores?: number;
  memoryBytes?: number;
  kernelImage?: string;
  registry?: string;
  platform?: string;
}

export class Sandbox {
  readonly client: RuntimeClient;
  id?: string;
  private readonly opts: SandboxOptions;

  constructor(
    target: string,
    readonly image: string,
    opts: SandboxOptions = {},
  ) {
    this.client = new RuntimeClient(target);
    this.opts = opts;
  }

  async start(): Promise<this> {
    const pull: any = await unary(this.client.client, "PullImage", {
      image_ref: this.image,
      registry: this.opts.registry ?? "",
      platform: this.opts.platform ?? "",
    });
    const req: Dict = {
      id: this.opts.name ?? `sdk-${randomUUID().replaceAll("-", "").slice(0, 12)}`,
      root_digest: pull.root_digest,
      backend: this.opts.backend ?? "container",
      command: this.opts.command ?? [],
      env: this.opts.env ?? {},
    };
    if (this.opts.network) req.network_mode = this.opts.network;
    if (this.opts.cpuMillicores) req.cpu_millicores = this.opts.cpuMillicores;
    if (this.opts.memoryBytes) req.memory_bytes = this.opts.memoryBytes;
    if (this.opts.kernelImage) req.kernel_image = this.opts.kernelImage;
    const run: any = await unary(this.client.client, "RunWorkload", req);
    this.id = run.id;
    return this;
  }

  private requireId(): string {
    if (!this.id) throw new Error("sandbox not started");
    return this.id;
  }

  async exec(command: string[], timeoutSeconds = 0): Promise<ExecResult> {
    const r: any = await unary(this.client.client, "ExecInWorkload", {
      id: this.requireId(),
      command,
      timeout_seconds: timeoutSeconds,
    });
    return { exitCode: r.exit_code, stdout: Buffer.from(r.stdout ?? []), stderr: Buffer.from(r.stderr ?? []) };
  }

  logs(follow = true, tail = 100): AsyncGenerator<Buffer> {
    const call = this.client.client.StreamLogs({ id: this.requireId(), follow, tail });
    return (async function* () {
      for await (const chunk of call) yield Buffer.from(chunk.data ?? []);
    })();
  }

  status(): Promise<any> {
    return unary(this.client.client, "GetWorkload", { id: this.requireId() });
  }

  snapshot(fidelity = "memory"): Promise<any> {
    return unary(this.client.client, "CreateSnapshot", {
      workload_id: this.requireId(),
      fidelity,
    });
  }

  async stop(): Promise<void> {
    if (!this.id) return;
    await unary(this.client.client, "StopWorkload", { id: this.id });
    this.id = undefined;
  }

  async close(): Promise<void> {
    try {
      await this.stop();
    } finally {
      this.client.close();
    }
  }
}

export async function restore(target: string, snapshotId: string, fork = false) {
  const client = new RuntimeClient(target);
  try {
    return (await unary(client.client, "RestoreSnapshot", {
      snapshot_id: snapshotId,
      fork,
    })) as { workload_id: string; backend: string; restore_ms: string };
  } finally {
    client.close();
  }
}
