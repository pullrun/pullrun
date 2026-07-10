// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "pullrun/protoapi/pullrun/runtime"

	"github.com/spf13/cobra"
)

func NewGCCommand(opts *RootOptions) *cobra.Command {
	var apply bool
	var force bool
	var verbose bool

	cmd := &cobra.Command{
		Use:   "gc",
		Short: "Garbage-collect unreachable nodes from the DAG store",
		Long: `Remove DAG nodes that are not reachable from any tagged image
or running workload root. By default performs a dry-run (only
reports what would be deleted). Use --apply to actually delete.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 300*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.Gc(ctx, &runtimepb.GcRequest{
				DryRun: !apply,
				Force:  force,
			})
			if err != nil {
				return fmt.Errorf("gc: %w", err)
			}

			if resp.Error != "" {
				fmt.Fprintf(cmd.ErrOrStderr(), "Error: %s\n", resp.Error)
				return nil
			}

			if verbose || !resp.DryRun {
				fmt.Printf("Total nodes:       %d\n", resp.TotalNodes)
				fmt.Printf("Reachable nodes:   %d\n", resp.ReachableNodes)
				fmt.Printf("Unreachable nodes: %d\n", resp.UnreachableNodes)
				fmt.Printf("Deleted nodes:     %d\n", resp.DeletedNodes)
				fmt.Printf("Deleted blobs:     %d\n", resp.DeletedBlobs)
				fmt.Printf("Bytes freed:       %d\n", resp.BytesFreed)
			}
			if resp.DryRun {
				if resp.UnreachableNodes > 0 {
					fmt.Printf("Would delete %d unreachable node(s). Re-run with --apply to proceed.\n", resp.UnreachableNodes)
				} else {
					fmt.Println("Nothing to delete.")
				}
			} else {
				fmt.Printf("GC complete: %d nodes deleted, %d bytes freed.\n", resp.DeletedNodes, resp.BytesFreed)
			}

			if verbose && len(resp.CollectedDigests) > 0 {
				fmt.Println("Deleted digests:")
				for _, d := range resp.CollectedDigests {
					fmt.Printf("  %s\n", d)
				}
			}
			return nil
		},
	}

	cmd.Flags().BoolVarP(&apply, "apply", "a", false, "Actually delete unreachable nodes (default: dry-run)")
	cmd.Flags().BoolVarP(&force, "force", "f", false, "Force GC even if the 90%% safety guard would block")
	cmd.Flags().BoolVarP(&verbose, "verbose", "v", false, "Show detailed per-node GC report")
	return cmd
}
