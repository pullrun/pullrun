package cmd

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"
)

func NewImagesCommand(opts *RootOptions) *cobra.Command {
	var asJSON bool

	cmd := &cobra.Command{
		Use:   "images",
		Short: "List pulled images in the local DAG store",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.ListImages(ctx)
			if err != nil {
				return fmt.Errorf("list images: %w", err)
			}

			if asJSON {
				enc := json.NewEncoder(os.Stdout)
				enc.SetIndent("", "  ")
				if err := enc.Encode(resp.Images); err != nil {
					return fmt.Errorf("encode JSON: %w", err)
				}
				return nil
			}

			if len(resp.Images) == 0 {
				fmt.Println("No images in store.")
				return nil
			}

			fmt.Printf("%-50s %-18s %-12s %s\n", "REFERENCE", "DIGEST (ROOT)", "SIZE", "CREATED")
			for _, img := range resp.Images {
				digest := img.RootDigest
				if len(digest) > 16 {
					digest = digest[:16] + "..."
				}
				size := formatBytes(img.SizeBytes)
				created := formatTimestamp(img.CreatedAt)
				fmt.Printf("%-50s %-18s %-12s %s\n", img.ImageRef, digest, size, created)
			}
			return nil
		},
	}

	cmd.Flags().BoolVar(&asJSON, "json", false, "Output as JSON")
	return cmd
}

func formatBytes(b int64) string {
	switch {
	case b >= 1<<30:
		return fmt.Sprintf("%.1f GiB", float64(b)/float64(1<<30))
	case b >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(b)/float64(1<<20))
	case b >= 1<<10:
		return fmt.Sprintf("%.1f KiB", float64(b)/float64(1<<10))
	default:
		return fmt.Sprintf("%d B", b)
	}
}

func formatTimestamp(unixSecs int64) string {
	if unixSecs == 0 {
		return "-"
	}
	return time.Unix(unixSecs, 0).Format(time.RFC3339)
}
