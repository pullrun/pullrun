package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "pullrun/protoapi/pullrun/runtime"

	"github.com/spf13/cobra"
)

func NewPruneCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "prune",
		Short: "Remove stopped workloads, stale bundles, and temp rootfs dirs",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.Prune(ctx, &runtimepb.PruneRequest{})
			if err != nil {
				return fmt.Errorf("prune: %w", err)
			}

			fmt.Printf("Bundles removed: %d\n", resp.BundlesRemoved)
			fmt.Printf("Bytes freed:     %d\n", resp.BytesFreed)
			if len(resp.Errors) > 0 {
				for _, e := range resp.Errors {
					fmt.Printf("Error: %s\n", e)
				}
			}
			return nil
		},
	}
}
