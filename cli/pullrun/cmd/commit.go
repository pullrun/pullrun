package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "pullrun/protoapi/pullrun/runtime"

	"github.com/spf13/cobra"
)

func NewCommitCommand(opts *RootOptions) *cobra.Command {
	var message string
	var author string

	cmd := &cobra.Command{
		Use:   "commit <workload_id> [tag]",
		Short: "Commit a running workload as a new image layer",
		Args:  cobra.MinimumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			id := args[0]
			tag := ""
			if len(args) > 1 {
				tag = args[1]
			}

			resp, err := client.CommitImage(ctx, &runtimepb.CommitImageRequest{
				Id:      id,
				Tag:     tag,
				Message: message,
				Author:  author,
			})
			if err != nil {
				return fmt.Errorf("commit: %w", err)
			}

			fmt.Printf("Committed: %s\n", resp.RootDigest)
			if resp.Tag != "" {
				fmt.Printf("Tag:       %s\n", resp.Tag)
			}
			fmt.Printf("New nodes: %d\n", resp.NewNodes)
			return nil
		},
	}

	cmd.Flags().StringVar(&message, "message", "", "Commit message")
	cmd.Flags().StringVar(&author, "author", "", "Author of the commit")
	return cmd
}
