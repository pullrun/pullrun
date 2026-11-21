// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// NewGroupCommand manages sandbox groups: one VM hosting multiple
// sandboxes that share the guest kernel page cache.
func NewGroupCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "group",
		Short: "Manage sandbox groups (one VM, many sandboxes)",
		Long: `A sandbox group boots one Apple Virtualization VM and runs
sandboxes inside it. Every sandbox sees a fresh writable overlay over
the read-only image base, while all of them share the guest kernel and
its page cache. Useful for agents that run many tasks from one image.`,
	}
	cmd.AddCommand(newGroupCreateCommand(opts))
	cmd.AddCommand(newGroupListCommand(opts))
	cmd.AddCommand(newGroupRemoveCommand(opts))
	cmd.AddCommand(newGroupRunCommand(opts))
	return cmd
}

func newGroupCreateCommand(opts *RootOptions) *cobra.Command {
	var image string
	var name string
	cmd := &cobra.Command{
		Use:   "create --image <ref|digest>",
		Short: "Boot a sandbox-group VM for an image",
		RunE: func(cmd *cobra.Command, _ []string) error {
			if image == "" {
				return fmt.Errorf("--image is required")
			}
			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer cleanup()

			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
			defer cancel()

			// Resolve a tag to its root digest first; groups are keyed
			// by the immutable digest.
			digest := image
			if !strings.HasPrefix(image, "sha256:") && !isHexDigest(image) {
				if _, err := client.PullImage(ctx, image, "", "", nil); err != nil {
					return fmt.Errorf("prepare image: %w", err)
				}
				images, err := client.ListImages(ctx)
				if err != nil {
					return err
				}
				for _, img := range images.Images {
					if img.ImageRef == image {
						digest = img.RootDigest
						break
					}
				}
			}

			resp, err := client.CreateGroup(ctx, digest, name)
			if err != nil {
				return fmt.Errorf("create group: %w", err)
			}
			fmt.Printf("%s\n", resp.GroupId)
			return nil
		},
	}
	cmd.Flags().StringVar(&image, "image", "", "Image reference or root digest")
	cmd.Flags().StringVar(&name, "name", "", "Optional group name")
	return cmd
}

func newGroupListCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:     "ls",
		Aliases: []string{"list"},
		Short:   "List sandbox groups",
		RunE: func(cmd *cobra.Command, _ []string) error {
			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer cleanup()
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()
			resp, err := client.ListGroups(ctx)
			if err != nil {
				return err
			}
			if len(resp.Groups) == 0 {
				fmt.Println("no sandbox groups")
				return nil
			}
			fmt.Printf("%-20s %-10s %-20s %s\n", "GROUP", "STATUS", "BACKEND", "IMAGE")
			for _, g := range resp.Groups {
				fmt.Printf("%-20s %-10s %-20s %s\n", g.GroupId, g.Status, g.Backend, g.ImageRoot)
			}
			return nil
		},
	}
}

func newGroupRemoveCommand(opts *RootOptions) *cobra.Command {
	var force bool
	cmd := &cobra.Command{
		Use:     "rm <group-id>",
		Aliases: []string{"remove"},
		Args:    cobra.ExactArgs(1),
		Short:   "Stop and remove a sandbox group",
		RunE: func(cmd *cobra.Command, args []string) error {
			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer cleanup()
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
			defer cancel()
			resp, err := client.RemoveGroup(ctx, args[0], force)
			if err != nil {
				return err
			}
			if !resp.Removed {
				return fmt.Errorf("group %s not found", args[0])
			}
			return nil
		},
	}
	cmd.Flags().BoolVar(&force, "force", false, "Force removal")
	return cmd
}

func newGroupRunCommand(opts *RootOptions) *cobra.Command {
	var envs []string
	var workingDir string
	cmd := &cobra.Command{
		Use:   "run <group-id> -- <command...>",
		Short: "Run a sandbox inside a group VM",
		Long: `Run one sandbox in the group VM. The command runs in an
isolated mount/network/UTS/IPC namespace with a fresh overlay over the
image; stdout, stderr, and the exit code are returned.`,
		Args: cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			groupID := args[0]
			command := args[1:]

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer cleanup()

			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
			defer cancel()

			resp, err := client.RunGroupSandbox(ctx, &runtimepb.RunGroupSandboxRequest{
				GroupId:    groupID,
				Command:    command,
				Env:        envs,
				WorkingDir: workingDir,
			})
			if err != nil {
				return fmt.Errorf("run sandbox: %w", err)
			}
			os.Stdout.Write(resp.Stdout)
			os.Stderr.Write(resp.Stderr)
			if resp.ExitCode != 0 {
				os.Exit(int(resp.ExitCode))
			}
			return nil
		},
	}
	cmd.Flags().StringArrayVarP(&envs, "env", "e", nil, "Environment variable (KEY=VALUE)")
	cmd.Flags().StringVarP(&workingDir, "workdir", "w", "", "Working directory inside the sandbox")
	return cmd
}

func isHexDigest(s string) bool {
	if len(s) != 64 {
		return false
	}
	for _, r := range s {
		if (r < '0' || r > '9') && (r < 'a' || r > 'f') {
			return false
		}
	}
	return true
}
