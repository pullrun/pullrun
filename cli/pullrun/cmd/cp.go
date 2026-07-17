// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

func NewCpCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "cp <workload_id>:<container_path> <local_path>",
		Short: "Copy files between a workload and the local filesystem",
		Long: `Copy files between a running workload and the local filesystem.
Like docker cp, but simplified for v0 (single file only, no tar archives).

Examples:
  pullrun cp my-app:/etc/nginx/nginx.conf ./nginx.conf   # copy out
  pullrun cp ./my-config.txt my-app:/etc/config.txt      # copy in`,
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			src := args[0]
			dst := args[1]

			// Determine direction: "out" (workload->local) or "in" (local->workload).
			// "out" if src has a workload prefix (id:path) and dst is a local path.
			// "in" if src is a local path and dst has a workload prefix.
			if isWorkloadPath(src) && !isWorkloadPath(dst) {
				return copyOut(ctx, client, src, dst)
			} else if !isWorkloadPath(src) && isWorkloadPath(dst) {
				return copyIn(ctx, client, src, dst)
			}
			return fmt.Errorf("invalid arguments: expected one workload path (id:path) and one local path")
		},
	}
}

func isWorkloadPath(p string) bool {
	// A workload path looks like "id:/path" or "id:relative/path".
	// It must contain a colon with a non-empty id before it.
	idx := strings.Index(p, ":")
	if idx <= 0 {
		return false
	}
	// The part before ':' should not be empty and should not look like a drive letter (Windows).
	prefix := p[:idx]
	if strings.Contains(prefix, "/") || strings.Contains(prefix, "\\") {
		return false
	}
	return true
}

func splitWorkloadPath(p string) (id, containerPath string) {
	idx := strings.Index(p, ":")
	return p[:idx], p[idx+1:]
}

func copyOut(ctx context.Context, client *GRPCClient, workloadPath, localPath string) error {
	id, containerPath := splitWorkloadPath(workloadPath)

	resp, err := client.CopyFile(ctx, &runtimepb.CopyFileRequest{
		Id:            id,
		ContainerPath: containerPath,
		Direction:     "out",
	})
	if err != nil {
		return fmt.Errorf("copy out: %w", err)
	}

	if err := os.WriteFile(localPath, resp.Content, os.FileMode(resp.Mode)); err != nil {
		return fmt.Errorf("write %s: %w", localPath, err)
	}

	abs, _ := filepath.Abs(localPath)
	fmt.Printf("Copied %d bytes from %s:%s to %s\n", resp.Size, id, containerPath, abs)
	return nil
}

func copyIn(ctx context.Context, client *GRPCClient, localPath, workloadPath string) error {
	id, containerPath := splitWorkloadPath(workloadPath)

	content, err := os.ReadFile(localPath)
	if err != nil {
		return fmt.Errorf("read %s: %w", localPath, err)
	}

	// Get file mode for preservation.
	info, err := os.Stat(localPath)
	if err != nil {
		return fmt.Errorf("stat %s: %w", localPath, err)
	}

	resp, err := client.CopyFile(ctx, &runtimepb.CopyFileRequest{
		Id:            id,
		ContainerPath: containerPath,
		Direction:     "in",
		Content:       content,
		Mode:          uint32(info.Mode().Perm()),
	})
	if err != nil {
		return fmt.Errorf("copy in: %w", err)
	}

	fmt.Printf("Copied %d bytes from %s to %s:%s\n", resp.Size, localPath, id, containerPath)
	return nil
}
