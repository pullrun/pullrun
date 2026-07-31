// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

// Package mcp implements an MCP (Model Context Protocol) server for pullrun.
//
// It exposes pullrun runtime operations as MCP tools so that AI agents
// (opencode, Claude Code, Cursor, etc.) can pull images, run workloads,
// exec into containers, inspect state, and manage the runtime — all
// through natural language.
//
// Architecture
//
//	AI agent (opencode, etc.)
//	 │  MCP stdio (default) or SSE
//	 ▼
//	 pullrun mcp              ← this server
//	 │  gRPC unix socket
//	 ▼
//	 pullrun-runtime          ← unchanged daemon
//
// Tools
//
// Workload lifecycle:
//   run           — Create and start a workload (container/VM)
//   stop          — Stop a running workload
//   exec          — Run a command inside a running workload
//   list          — List all workloads
//   get           — Get a workload's current status
//   inspect       — Deep-inspect a workload (layers, policy, network)
//   logs          — Retrieve recent log output from a workload
//   stats         — Live resource statistics for a workload
//
// Image management:
//   pull_image    — Pull an OCI image from a registry
//   list_images   — List images in the local DAG store
//   build         — Build an OCI image from a Dockerfile
//   push          — Push a local image to a registry (takes root_digest + target)
//   prune         — Garbage-collect unused DAG nodes
//
// Compose / orchestration:
//   compose_up    — Deploy workloads from a compose file
//   compose_down  — Tear down workloads defined in a compose file
//
// Resources
//
//	pullrun://workload/{id}          → workload status (JSON)
//	pullrun://workload/{id}/logs     → log output (text)
//	pullrun://store/info             → store statistics (JSON)
//	pullrun://images                 → image list (JSON)
package mcp

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// Server wraps an MCP server with pullrun tool implementations.
type Server struct {
	mcp    *server.MCPServer
	client GRPCClientProvider
}

// GRPCClientProvider abstracts the gRPC connection so we can mock it in tests.
// The adapter in the cmd package (*mcpGRPCClientAdapter) bridges the real
// *cmd.GRPCClient (which uses cmd.RegistryAuth) to this interface.
type GRPCClientProvider interface {
	PullImage(ctx context.Context, imageRef, registry, platform string) (*runtimepb.PullImageResponse, error)
	RunWorkload(ctx context.Context, req *runtimepb.RunRequest) (*runtimepb.RunResponse, error)
	StopWorkload(ctx context.Context, id string) (*runtimepb.StopResponse, error)
	GetWorkload(ctx context.Context, id string) (*runtimepb.WorkloadStatus, error)
	ListWorkloads(ctx context.Context) (*runtimepb.ListWorkloadsResponse, error)
	InspectWorkload(ctx context.Context, id string) (*runtimepb.InspectResponse, error)
	StreamLogs(ctx context.Context, id string, follow bool, tail int64) (runtimepb.Runtime_StreamLogsClient, error)
	ExecInWorkload(ctx context.Context, id string, command []string) (*runtimepb.ExecResponse, error)
	BuildImage(ctx context.Context, req *runtimepb.BuildImageRequest) (*runtimepb.BuildImageResponse, error)
	PushImage(ctx context.Context, rootDigest, targetRef string) (*runtimepb.PushImageResponse, error)
	Prune(ctx context.Context, req *runtimepb.PruneRequest) (*runtimepb.PruneResponse, error)
	GetWorkloadStats(ctx context.Context, req *runtimepb.GetWorkloadStatsRequest) (*runtimepb.WorkloadStats, error)
	RuntimeInfo(ctx context.Context, req *runtimepb.InfoRequest) (*runtimepb.InfoResponse, error)
	ListImages(ctx context.Context) (*runtimepb.ListImagesResponse, error)
}

func NewServer(client GRPCClientProvider) *Server {
	s := &Server{
		client: client,
	}

	mcpSrv := server.NewMCPServer(
		"pullrun",
		"0.7.8",
		server.WithResourceCapabilities(true, false),
		server.WithLogging(),
	)

	s.registerTools(mcpSrv)
	s.registerResources(mcpSrv)

	s.mcp = mcpSrv
	return s
}

// ServeStdio starts the server in stdio mode (stdin/stdout).
func (s *Server) ServeStdio() error {
	return server.ServeStdio(s.mcp)
}

// ServeSSE starts the server in SSE (HTTP) mode.
func (s *Server) ServeSSE(addr string) error {
	sseSrv := server.NewSSEServer(s.mcp)
	return sseSrv.Start(addr)
}

// MCPServer returns the underlying MCP server (for advanced usage).
func (s *Server) MCPServer() *server.MCPServer {
	return s.mcp
}

// registerTools defines all MCP tools and their handlers.
func (s *Server) registerTools(mcpSrv *server.MCPServer) {
	tools := []struct {
		tool    mcp.Tool
		handler server.ToolHandlerFunc
	}{
		{tool: s.defineRun(), handler: s.handleRun},
		{tool: s.defineStop(), handler: s.handleStop},
		{tool: s.defineExec(), handler: s.handleExec},
		{tool: s.defineList(), handler: s.handleList},
		{tool: s.defineGet(), handler: s.handleGet},
		{tool: s.defineInspect(), handler: s.handleInspect},
		{tool: s.defineLogs(), handler: s.handleLogs},
		{tool: s.defineStats(), handler: s.handleStats},
		{tool: s.definePullImage(), handler: s.handlePullImage},
		{tool: s.defineListImages(), handler: s.handleListImages},
		{tool: s.defineBuild(), handler: s.handleBuild},
		{tool: s.definePush(), handler: s.handlePush},
		{tool: s.definePrune(), handler: s.handlePrune},
		{tool: s.defineComposeUp(), handler: s.handleComposeUp},
		{tool: s.defineComposeDown(), handler: s.handleComposeDown},
	}

	for _, t := range tools {
		mcpSrv.AddTool(t.tool, t.handler)
	}
}

// registerResources defines MCP resource providers.
func (s *Server) registerResources(mcpSrv *server.MCPServer) {
	mcpSrv.AddResource(
		mcp.NewResource(
			"pullrun://workload/{id}",
			"Workload Status",
			mcp.WithResourceDescription("Current status of a workload (JSON)"),
			mcp.WithMIMEType("application/json"),
		),
		s.handleWorkloadResource,
	)

	mcpSrv.AddResource(
		mcp.NewResource(
			"pullrun://workload/{id}/logs",
			"Workload Logs",
			mcp.WithResourceDescription("Recent log output from a workload"),
			mcp.WithMIMEType("text/plain"),
		),
		s.handleLogsResource,
	)

	mcpSrv.AddResource(
		mcp.NewResource(
			"pullrun://store/info",
			"Store Information",
			mcp.WithResourceDescription("DAG store statistics (JSON)"),
			mcp.WithMIMEType("application/json"),
		),
		s.handleStoreResource,
	)

	mcpSrv.AddResource(
		mcp.NewResource(
			"pullrun://images",
			"Image List",
			mcp.WithResourceDescription("List of images in the local DAG store (JSON)"),
			mcp.WithMIMEType("application/json"),
		),
		s.handleImagesResource,
	)
}

// ─── Tool definitions ────────────────────────────────────────────

func (s *Server) defineRun() mcp.Tool {
	return mcp.NewTool("run",
		mcp.WithDescription("Create and start a workload (container or VM)"),
		mcp.WithString("image",
			mcp.Required(),
			mcp.Description("OCI image reference (e.g., 'alpine:latest')"),
		),
		mcp.WithString("id",
			mcp.Description("Workload ID (auto-generated if omitted)"),
		),
		mcp.WithString("command",
			mcp.Description("Command to run (space-separated, e.g., '/bin/echo hello')"),
		),
		mcp.WithArray("env",
			mcp.Description("Environment variables (array of KEY=VALUE strings)"),
		),
		mcp.WithString("backend",
			mcp.Description("Backend: 'container' (default) or 'vm'"),
		),
		mcp.WithNumber("cpus",
			mcp.Description("CPU count (e.g., 2)"),
		),
		mcp.WithNumber("memory",
			mcp.Description("Memory limit in MiB (e.g., 512)"),
		),
		mcp.WithString("registry",
			mcp.Description("Registry host (defaults to Docker Hub)"),
		),
		mcp.WithString("platform",
			mcp.Description("Platform (e.g., 'linux/amd64', 'linux/arm64')"),
		),
	)
}

func (s *Server) defineStop() mcp.Tool {
	return mcp.NewTool("stop",
		mcp.WithDescription("Stop a running workload"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
	)
}

func (s *Server) defineExec() mcp.Tool {
	return mcp.NewTool("exec",
		mcp.WithDescription("Run a command inside a running workload"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
		mcp.WithString("command",
			mcp.Required(),
			mcp.Description("Command to run (e.g., 'ls -la /')"),
		),
	)
}

func (s *Server) defineList() mcp.Tool {
	return mcp.NewTool("list",
		mcp.WithDescription("List all workloads with their status"),
	)
}

func (s *Server) defineGet() mcp.Tool {
	return mcp.NewTool("get",
		mcp.WithDescription("Get detailed status of a single workload"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
	)
}

func (s *Server) defineInspect() mcp.Tool {
	return mcp.NewTool("inspect",
		mcp.WithDescription("Deep-inspect a workload (state, layers, network, policy)"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
	)
}

func (s *Server) defineLogs() mcp.Tool {
	return mcp.NewTool("logs",
		mcp.WithDescription("Retrieve recent log output from a workload"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
		mcp.WithNumber("tail",
			mcp.Description("Number of recent lines (default 50)"),
		),
	)
}

func (s *Server) defineStats() mcp.Tool {
	return mcp.NewTool("stats",
		mcp.WithDescription("Get live resource statistics for a running workload"),
		mcp.WithString("id", mcp.Required(), mcp.Description("Workload ID")),
	)
}

func (s *Server) definePullImage() mcp.Tool {
	return mcp.NewTool("pull_image",
		mcp.WithDescription("Pull an OCI image from a registry into the local DAG store"),
		mcp.WithString("image", mcp.Required(), mcp.Description("Image reference (e.g., 'alpine:latest')")),
		mcp.WithString("registry", mcp.Description("Registry host (defaults to Docker Hub)")),
		mcp.WithString("platform", mcp.Description("Platform (e.g., 'linux/amd64', 'linux/arm64')")),
	)
}

func (s *Server) defineListImages() mcp.Tool {
	return mcp.NewTool("list_images",
		mcp.WithDescription("List images in the local DAG store"),
	)
}

func (s *Server) defineBuild() mcp.Tool {
	return mcp.NewTool("build",
		mcp.WithDescription("Build an OCI image from a Dockerfile"),
		mcp.WithString("dockerfile",
			mcp.Required(),
			mcp.Description("Path to Dockerfile or directory containing one"),
		),
		mcp.WithString("tag", mcp.Description("Image tag (e.g., 'myapp:latest')")),
		mcp.WithString("platform",
			mcp.Description("Target platform (e.g., 'linux/amd64', 'linux/arm64')"),
		),
		mcp.WithArray("build_arg",
			mcp.Description("Build arguments (array of KEY=VALUE strings)"),
		),
		mcp.WithBoolean("push",
			mcp.Description("Push the built image to the registry after build"),
		),
	)
}

func (s *Server) definePush() mcp.Tool {
	return mcp.NewTool("push",
		mcp.WithDescription("Push a local image to a registry"),
		mcp.WithString("root_digest", mcp.Required(), mcp.Description("Root digest of the image (e.g., 'sha256:abc123...')")),
		mcp.WithString("target", mcp.Required(), mcp.Description("Target reference (e.g., 'registry.example.com/repo:tag')")),
	)
}

func (s *Server) definePrune() mcp.Tool {
	return mcp.NewTool("prune",
		mcp.WithDescription("Garbage-collect unused DAG nodes and free disk space"),
	)
}

func (s *Server) defineComposeUp() mcp.Tool {
	return mcp.NewTool("compose_up",
		mcp.WithDescription("Deploy workloads from a compose file"),
		mcp.WithString("file", mcp.Required(), mcp.Description("Path to compose file")),
	)
}

func (s *Server) defineComposeDown() mcp.Tool {
	return mcp.NewTool("compose_down",
		mcp.WithDescription("Tear down workloads defined in a compose file"),
		mcp.WithString("file", mcp.Description("Path to compose file (tears down all if omitted)")),
	)
}

// ─── Tool handlers ───────────────────────────────────────────────

func (s *Server) handleRun(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	image, _ := args["image"].(string)
	if image == "" {
		return mcp.NewToolResultError("'image' is required"), nil
	}

	rootDigest := image
	if !strings.HasPrefix(rootDigest, "sha256:") {
		registry, _ := args["registry"].(string)
		platform, _ := args["platform"].(string)
		pullResp, err := s.client.PullImage(ctx, image, registry, platform)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("pull %s: %v", image, err)), nil
		}
		rootDigest = pullResp.RootDigest
	}

	r := &runtimepb.RunRequest{
		RootDigest: rootDigest,
	}

	if id, ok := args["id"].(string); ok && id != "" {
		r.Id = id
	}
	if cmd, ok := args["command"].(string); ok && cmd != "" {
		r.Command = strings.Fields(cmd)
	}
	if envRaw, ok := args["env"].([]interface{}); ok {
		envMap := make(map[string]string)
		for _, e := range envRaw {
			if s, ok := e.(string); ok {
				if k, v, ok := strings.Cut(s, "="); ok {
					envMap[k] = v
				}
			}
		}
		r.Env = envMap
	}
	if backend, ok := args["backend"].(string); ok && backend != "" {
		r.Backend = backend
	}
	if cpus, ok := args["cpus"].(float64); ok && cpus > 0 {
		r.CpuMillicores = uint64(cpus * 1000)
	}
	if mem, ok := args["memory"].(float64); ok && mem > 0 {
		r.MemoryBytes = uint64(mem * 1024 * 1024)
	}

	resp, err := s.client.RunWorkload(ctx, r)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("run failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleStop(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}

	resp, err := s.client.StopWorkload(ctx, id)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("stop failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleExec(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	cmdRaw, _ := args["command"].(string)

	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}
	if cmdRaw == "" {
		return mcp.NewToolResultError("'command' is required"), nil
	}

	cmdParts := strings.Fields(cmdRaw)
	resp, err := s.client.ExecInWorkload(ctx, id, cmdParts)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("exec failed: %v", err)), nil
	}

	out := string(resp.Stdout)
	if len(resp.Stderr) > 0 {
		if out != "" {
			out += "\n"
		}
		out += string(resp.Stderr)
	}
	return mcp.NewToolResultText(out), nil
}

func (s *Server) handleList(ctx context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	resp, err := s.client.ListWorkloads(ctx)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("list failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleGet(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}

	resp, err := s.client.GetWorkload(ctx, id)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("get failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleInspect(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}

	resp, err := s.client.InspectWorkload(ctx, id)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("inspect failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleLogs(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}

	tail := int64(50)
	if t, ok := args["tail"].(float64); ok && t > 0 {
		tail = int64(t)
	}

	stream, err := s.client.StreamLogs(ctx, id, false, tail)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("logs failed: %v", err)), nil
	}

	var output strings.Builder
	for {
		chunk, err := stream.Recv()
		if err != nil {
			break
		}
		output.Write(chunk.Data)
	}

	return mcp.NewToolResultText(output.String()), nil
}

func (s *Server) handleStats(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	id, _ := args["id"].(string)
	if id == "" {
		return mcp.NewToolResultError("'id' is required"), nil
	}

	stats, err := s.client.GetWorkloadStats(ctx, &runtimepb.GetWorkloadStatsRequest{Id: id})
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("stats failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(stats, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handlePullImage(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	image, _ := args["image"].(string)
	if image == "" {
		return mcp.NewToolResultError("'image' is required"), nil
	}

	registry, _ := args["registry"].(string)
	platform, _ := args["platform"].(string)

	resp, err := s.client.PullImage(ctx, image, registry, platform)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("pull failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleListImages(ctx context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	resp, err := s.client.ListImages(ctx)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("list images failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleBuild(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	dockerfile, _ := args["dockerfile"].(string)
	if dockerfile == "" {
		return mcp.NewToolResultError("'dockerfile' is required"), nil
	}

	tag, _ := args["tag"].(string)
	platform, _ := args["platform"].(string)
	push, _ := args["push"].(bool)

	buildArgs := make(map[string]string)
	if buildRaw, ok := args["build_arg"].([]interface{}); ok {
		for _, e := range buildRaw {
			if s, ok := e.(string); ok {
				if k, v, ok := strings.Cut(s, "="); ok {
					buildArgs[k] = v
				}
			}
		}
	}

	resp, err := s.client.BuildImage(ctx, &runtimepb.BuildImageRequest{
		Dockerfile: dockerfile,
		Tag:        tag,
		Platform:   platform,
		BuildArgs:  buildArgs,
		Push:       push,
	})
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("build failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handlePush(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	rootDigest, _ := args["root_digest"].(string)
	if rootDigest == "" {
		return mcp.NewToolResultError("'root_digest' is required"), nil
	}

	target, _ := args["target"].(string)
	if target == "" {
		return mcp.NewToolResultError("'target' is required"), nil
	}

	resp, err := s.client.PushImage(ctx, rootDigest, target)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("push failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handlePrune(ctx context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	resp, err := s.client.Prune(ctx, &runtimepb.PruneRequest{})
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("prune failed: %v", err)), nil
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return mcp.NewToolResultText(string(b)), nil
}

func (s *Server) handleComposeUp(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	args := req.GetArguments()
	file, _ := args["file"].(string)
	if file == "" {
		return mcp.NewToolResultError("'file' is required"), nil
	}

	return mcp.NewToolResultError("compose_up is not available via the MCP API yet; use the CLI directly"), nil
}

func (s *Server) handleComposeDown(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	return mcp.NewToolResultError("compose_down is not available via the MCP API yet; use the CLI directly"), nil
}

// ─── Resource handlers ───────────────────────────────────────────

func (s *Server) handleWorkloadResource(ctx context.Context, req mcp.ReadResourceRequest) ([]mcp.ResourceContents, error) {
	id := extractID(req.Params.URI, "workload")
	if id == "" {
		return nil, fmt.Errorf("invalid URI: %s", req.Params.URI)
	}

	status, err := s.client.GetWorkload(ctx, id)
	if err != nil {
		return nil, fmt.Errorf("get workload %s: %w", id, err)
	}

	b, _ := json.MarshalIndent(status, "", "  ")
	return []mcp.ResourceContents{
		mcp.TextResourceContents{
			URI:      req.Params.URI,
			MIMEType: "application/json",
			Text:     string(b),
		},
	}, nil
}

func (s *Server) handleLogsResource(ctx context.Context, req mcp.ReadResourceRequest) ([]mcp.ResourceContents, error) {
	id := extractID(req.Params.URI, "logs")
	if id == "" {
		return nil, fmt.Errorf("invalid URI: %s", req.Params.URI)
	}

	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	stream, err := s.client.StreamLogs(ctx, id, false, 100)
	if err != nil {
		return nil, fmt.Errorf("stream logs %s: %w", id, err)
	}

	var out strings.Builder
	for {
		chunk, err := stream.Recv()
		if err != nil {
			break
		}
		out.Write(chunk.Data)
	}

	return []mcp.ResourceContents{
		mcp.TextResourceContents{
			URI:      req.Params.URI,
			MIMEType: "text/plain",
			Text:     out.String(),
		},
	}, nil
}

func (s *Server) handleStoreResource(ctx context.Context, _ mcp.ReadResourceRequest) ([]mcp.ResourceContents, error) {
	info, err := s.client.RuntimeInfo(ctx, &runtimepb.InfoRequest{})
	if err != nil {
		return nil, fmt.Errorf("runtime info: %w", err)
	}

	b, _ := json.MarshalIndent(info, "", "  ")
	return []mcp.ResourceContents{
		mcp.TextResourceContents{
			URI:      "pullrun://store/info",
			MIMEType: "application/json",
			Text:     string(b),
		},
	}, nil
}

func (s *Server) handleImagesResource(ctx context.Context, _ mcp.ReadResourceRequest) ([]mcp.ResourceContents, error) {
	resp, err := s.client.ListImages(ctx)
	if err != nil {
		return nil, fmt.Errorf("list images: %w", err)
	}

	b, _ := json.MarshalIndent(resp, "", "  ")
	return []mcp.ResourceContents{
		mcp.TextResourceContents{
			URI:      "pullrun://images",
			MIMEType: "application/json",
			Text:     string(b),
		},
	}, nil
}

// extractID parses workload/{id} or workload/{id}/logs from a URI.
func extractID(uri, suffix string) string {
	// Expected: pullrun://workload/{id} or pullrun://workload/{id}/logs
	trimmed := strings.TrimPrefix(uri, "pullrun://workload/")
	if suffix != "" {
		trimmed = strings.TrimSuffix(trimmed, "/"+suffix)
	}
	return trimmed
}


