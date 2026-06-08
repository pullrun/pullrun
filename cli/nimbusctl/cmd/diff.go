package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "nimbus/protoapi/nimbus/runtime"

	"github.com/spf13/cobra"
)

func NewDiffCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "diff <workload_id>",
		Short: "Show file changes in a running workload vs its original image",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.DiffWorkload(ctx, &runtimepb.DiffRequest{
				Id: args[0],
			})
			if err != nil {
				return fmt.Errorf("diff: %w", err)
			}

			if len(resp.Added) > 0 {
				fmt.Println("A (added):")
				for _, f := range resp.Added {
					fmt.Printf("  %s\n", f)
				}
			}
			if len(resp.Deleted) > 0 {
				fmt.Println("D (deleted):")
				for _, f := range resp.Deleted {
					fmt.Printf("  %s\n", f)
				}
			}
			if len(resp.Modified) > 0 {
				fmt.Println("M (modified):")
				for _, f := range resp.Modified {
					fmt.Printf("  %s\n", f)
				}
			}
			if len(resp.Added) == 0 && len(resp.Deleted) == 0 && len(resp.Modified) == 0 {
				fmt.Println("No changes.")
			}
			return nil
		},
	}
}
