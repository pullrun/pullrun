# MCP Server Reference

The MCP (Model Context Protocol) server exposes pullrun runtime operations as
tools so that AI agents (opencode, Claude Code, Cursor, etc.) can pull images,
run workloads, exec into containers, inspect state, and manage the runtime --
all through natural language.

## Architecture

```
AI agent (opencode, etc.)
 │  MCP stdio (default) or SSE
 ▼
 pullrun mcp              ← this server
 │  gRPC unix socket
 ▼
 pullrun-runtime          ← unchanged daemon
```

## Starting the server

```bash
# Stdio mode (default) -- for opencode, Claude Code, Cursor
pullrun mcp

# SSE (HTTP) mode -- for remote agents
pullrun mcp --sse :8080
```

### opencode configuration

Add to your `opencode.json` or `.opencode.json`:

```json
{
  "mcpServers": {
    "pullrun": {
      "command": "pullrun",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code configuration

Add to `~/.claude.json`:

```json
{
  "mcpServers": {
    "pullrun": {
      "command": "pullrun",
      "args": ["mcp"]
    }
  }
}
```

## Tools

### Workload lifecycle

#### `run`
Create and start a workload (container or VM).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `image` | string | yes | OCI image reference (e.g. `alpine:latest`) |
| `id` | string | no | Workload ID (auto-generated if omitted) |
| `command` | string | no | Command to run (space-separated) |
| `env` | array | no | Environment variables (KEY=VALUE strings) |
| `backend` | string | no | Backend: `container` (default) or `vm` |
| `cpus` | number | no | CPU count (e.g. `2`) |
| `memory` | number | no | Memory limit in MiB (e.g. `512`) |
| `registry` | string | no | Registry host (defaults to Docker Hub) |
| `platform` | string | no | Platform (e.g. `linux/amd64`) |

#### `stop`
Stop a running workload.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |

#### `exec`
Run a command inside a running workload.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |
| `command` | string | yes | Command to run (e.g. `ls -la /`) |

#### `list`
List all workloads with their status. No parameters.

#### `get`
Get detailed status of a single workload.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |

#### `inspect`
Deep-inspect a workload (state, layers, network, policy).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |

#### `logs`
Retrieve recent log output from a workload.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |
| `tail` | number | no | Number of recent lines (default 50) |

#### `stats`
Get live resource statistics for a running workload.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Workload ID |

### Image management

#### `pull_image`
Pull an OCI image from a registry into the local DAG store.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `image` | string | yes | Image reference (e.g. `alpine:latest`) |
| `registry` | string | no | Registry host (defaults to Docker Hub) |
| `platform` | string | no | Platform (e.g. `linux/amd64`) |

#### `list_images`
List images in the local DAG store. No parameters.

#### `build`
Build an OCI image from a Dockerfile.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `dockerfile` | string | yes | Path to Dockerfile or directory containing one |
| `tag` | string | no | Image tag (e.g. `myapp:latest`) |

#### `push`
Push a local image to a registry.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `image` | string | yes | Image reference (e.g. `myapp:latest`) |
| `registry` | string | no | Target registry |

#### `prune`
Garbage-collect unused DAG nodes and free disk space. No parameters.

### Compose / orchestration

**Note:** `compose_up` and `compose_down` are declared but not yet implemented
via the MCP API. Use the CLI directly.

## Resources

The server exposes the following read-only resources:

| Resource URI | MIME Type | Description |
|-------------|-----------|-------------|
| `pullrun://workload/{id}` | application/json | Current status of a workload |
| `pullrun://workload/{id}/logs` | text/plain | Recent log output from a workload |
| `pullrun://store/info` | application/json | DAG store statistics |
| `pullrun://images` | application/json | List of images in the local DAG store |

## Example prompts

```
Pull the latest Alpine image.
List all running workloads.
Run a container from alpine:latest with 512 MB memory.
Run alpine:latest with the command 'echo hello world'.
List the image I just pulled.
Exec into workload wl-abc123 and run 'cat /etc/os-release'.
Show me the logs from workload wl-abc123.
Inspect workload wl-abc123 in detail.
Stop workload wl-abc123.
List workloads again to confirm it stopped.
```
