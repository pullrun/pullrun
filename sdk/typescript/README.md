# Pullrun TypeScript SDK

Thin client for the Pullrun runtime gRPC API. It does not start the daemon.

```bash
npm install
npm run build
```

```ts
import { Sandbox } from "@pullrun/sdk";

const sb = await new Sandbox("/tmp/pullrun.sock", "alpine:3.18", {
  backend: "container",
  command: ["sh", "-c", "echo hello"],
}).start();
console.log((await sb.exec(["echo", "sdk-ok"])).stdout.toString());
await sb.close();
```
