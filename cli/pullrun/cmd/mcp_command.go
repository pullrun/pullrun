package cmd

import (
	"context"
	"fmt"
	"os"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"

	"pullrun/cli/cmd/mcp"
)

// mcpGRPCClientAdapter wraps *GRPCClient to satisfy mcp.GRPCClientProvider.
// The adapter strips the *cmd.RegistryAuth parameter since MCP agents never
// pass credentials (auth comes from the host's stored config).
type mcpGRPCClientAdapter struct {
	inner *GRPCClient
}

func (a *mcpGRPCClientAdapter) PullImage(ctx context.Context, imageRef, registry, platform string) (*runtimepb.PullImageResponse, error) {
	return a.inner.PullImage(ctx, imageRef, registry, platform, nil)
}

func (a *mcpGRPCClientAdapter) RunWorkload(ctx context.Context, req *runtimepb.RunRequest) (*runtimepb.RunResponse, error) {
	return a.inner.RunWorkload(ctx, req)
}

func (a *mcpGRPCClientAdapter) StopWorkload(ctx context.Context, id string) (*runtimepb.StopResponse, error) {
	return a.inner.StopWorkload(ctx, id)
}

func (a *mcpGRPCClientAdapter) GetWorkload(ctx context.Context, id string) (*runtimepb.WorkloadStatus, error) {
	return a.inner.GetWorkload(ctx, id)
}

func (a *mcpGRPCClientAdapter) ListWorkloads(ctx context.Context) (*runtimepb.ListWorkloadsResponse, error) {
	return a.inner.ListWorkloads(ctx)
}

func (a *mcpGRPCClientAdapter) InspectWorkload(ctx context.Context, id string) (*runtimepb.InspectResponse, error) {
	return a.inner.InspectWorkload(ctx, id)
}

func (a *mcpGRPCClientAdapter) StreamLogs(ctx context.Context, id string, follow bool, tail int64) (runtimepb.Runtime_StreamLogsClient, error) {
	return a.inner.StreamLogs(ctx, id, follow, tail)
}

func (a *mcpGRPCClientAdapter) ExecInWorkload(ctx context.Context, id string, command []string) (*runtimepb.ExecResponse, error) {
	return a.inner.ExecInWorkload(ctx, id, command)
}

func (a *mcpGRPCClientAdapter) BuildImage(ctx context.Context, req *runtimepb.BuildImageRequest) (*runtimepb.BuildImageResponse, error) {
	return a.inner.BuildImage(ctx, req)
}

func (a *mcpGRPCClientAdapter) PushImage(ctx context.Context, rootDigest, targetRef string) (*runtimepb.PushImageResponse, error) {
	return a.inner.PushImage(ctx, rootDigest, targetRef, nil)
}

func (a *mcpGRPCClientAdapter) Prune(ctx context.Context, req *runtimepb.PruneRequest) (*runtimepb.PruneResponse, error) {
	return a.inner.Prune(ctx, req)
}

func (a *mcpGRPCClientAdapter) GetWorkloadStats(ctx context.Context, req *runtimepb.GetWorkloadStatsRequest) (*runtimepb.WorkloadStats, error) {
	return a.inner.GetWorkloadStats(ctx, req)
}

func (a *mcpGRPCClientAdapter) RuntimeInfo(ctx context.Context, req *runtimepb.InfoRequest) (*runtimepb.InfoResponse, error) {
	return a.inner.RuntimeInfo(ctx, req)
}

func (a *mcpGRPCClientAdapter) ListImages(ctx context.Context) (*runtimepb.ListImagesResponse, error) {
	return a.inner.ListImages(ctx)
}

// NewMCPCommand creates the cobra command that starts the MCP server.
func NewMCPCommand(opts *RootOptions) *cobra.Command {
	var sseAddr string

	cmd := &cobra.Command{
		Use:   "mcp",
		Short: "Start an MCP (Model Context Protocol) server for AI agents",
		Long: `Start an MCP server that exposes pullrun runtime operations as tools
so that AI agents (opencode, Claude Code, Cursor, etc.) can pull images,
run workloads, exec into containers, inspect state, and manage the runtime.

Uses stdio transport by default (for opencode, Claude Code). Pass --sse to
serve over HTTP for remote agents.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect to runtime: %w", err)
			}
			defer cleanup()

			srv := mcp.NewServer(&mcpGRPCClientAdapter{inner: client})

			if sseAddr != "" {
				fmt.Fprintf(os.Stderr, "MCP SSE server listening on %s\n", sseAddr)
				return srv.ServeSSE(sseAddr)
			}

			return srv.ServeStdio()
		},
	}

	cmd.Flags().StringVar(&sseAddr, "sse", "", "Serve over SSE on the given address (e.g., :8080)")
	return cmd
}
