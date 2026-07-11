package cmd

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"
)

func NewRmiCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "rmi TAG [TAG...]",
		Short: "Remove one or more images by tag or digest",
		Long: `Remove one or more images from the local DAG store.
Accepts OCI image tags (e.g. alpine:latest) or hex digest roots.

Shared layers are preserved: a layer referenced by another
image is not deleted until the last referencing image is removed.`,
		Args: cobra.MinimumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 300*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			// Fetch image list once to resolve tags.
			listResp, err := client.ListImages(ctx)
			if err != nil {
				return fmt.Errorf("list images: %w", err)
			}

			// Build tag -> digest map.
			tagToDigest := make(map[string]string)
			digestToTags := make(map[string][]string)
			for _, img := range listResp.Images {
				tagToDigest[img.ImageRef] = img.RootDigest
				digestToTags[img.RootDigest] = append(digestToTags[img.RootDigest], img.ImageRef)
			}

			type removal struct {
				arg    string
				digest string
			}
			var toRemove []removal

			for _, arg := range args {
				arg = strings.TrimSpace(arg)
				if arg == "" {
					continue
				}

				// Direct digest match (hex or sha256: prefixed).
				digestStr := strings.TrimPrefix(arg, "sha256:")
				if _, isDigest := digestToTags[digestStr]; isDigest {
					toRemove = append(toRemove, removal{arg: arg, digest: digestStr})
					continue
				}
				if _, ok := digestToTags[arg]; ok {
					toRemove = append(toRemove, removal{arg: arg, digest: arg})
					continue
				}

				// Tag lookup.
				if d, ok := tagToDigest[arg]; ok {
					toRemove = append(toRemove, removal{arg: arg, digest: d})
					continue
				}

				// Try matching just the tag portion (e.g. "latest" for "library/alpine:latest").
				var found bool
				for ref, d := range tagToDigest {
					if strings.HasSuffix(ref, ":"+arg) || strings.HasSuffix(ref, "/"+arg) {
						toRemove = append(toRemove, removal{arg: arg, digest: d})
						found = true
						break
					}
				}
				if found {
					continue
				}

				return fmt.Errorf("image %q not found", arg)
			}

			var totalFreed int64
			var failures int
			for _, r := range toRemove {
				tags := digestToTags[r.digest]
				tagStr := "untagged"
				if len(tags) > 0 {
					tagStr = strings.Join(tags, ", ")
				}

				resp, err := client.RemoveImage(ctx, r.digest)
				if err != nil {
					fmt.Printf("Removing %s (%s): error: %v\n", r.arg, tagStr, err)
					failures++
					continue
				}
				totalFreed += resp.BytesFreed
				fmt.Printf("Removed %s (%s): %d bytes freed\n", r.arg, tagStr, resp.BytesFreed)
			}
			if totalFreed > 0 {
				fmt.Printf("Total freed: %d bytes\n", totalFreed)
			}
			if failures > 0 {
				return fmt.Errorf("%d removal(s) failed", failures)
			}
			return nil
		},
	}
	return cmd
}
