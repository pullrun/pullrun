// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/spf13/cobra"
)

func NewConfigCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "config",
		Short: "Manage configs (docker config equivalent)",
	}

	cmd.AddCommand(&cobra.Command{
		Use:   "create [NAME] [FILE|-]",
		Short: "Create a config from a file or stdin",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			name := args[0]
			src := args[1]

			var data []byte
			if src == "-" {
				var err error
				data, err = io.ReadAll(os.Stdin)
				if err != nil {
					return fmt.Errorf("read stdin: %w", err)
				}
			} else {
				var err error
				data, err = os.ReadFile(src)
				if err != nil {
					return fmt.Errorf("read %s: %w", src, err)
				}
			}

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			_, err = client.CreateConfig(ctx, name, data)
			if err != nil {
				return fmt.Errorf("create config: %w", err)
			}
			fmt.Printf("✓ config %q created (%d bytes)\n", name, len(data))
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "ls",
		Short: "List configs",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.ListConfigs(ctx)
			if err != nil {
				return fmt.Errorf("list configs: %w", err)
			}
			if len(resp.Configs) == 0 {
				fmt.Println("No configs.")
				return nil
			}
			fmt.Printf("%-24s %10s  %s\n", "NAME", "SIZE", "CREATED")
			for _, c := range resp.Configs {
				t := time.Unix(c.CreatedAt, 0).Format(time.RFC3339)
				fmt.Printf("%-24s %10d  %s\n", c.Name, c.SizeBytes, t)
			}
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "inspect [NAME]",
		Short: "Inspect a config",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.InspectConfig(ctx, args[0])
			if err != nil {
				return fmt.Errorf("inspect config: %w", err)
			}
			c := resp.Config
			if c == nil {
				fmt.Printf("Config %q not found.\n", args[0])
				return nil
			}
			t := time.Unix(c.CreatedAt, 0).Format(time.RFC3339)
			fmt.Printf("Name:       %s\n", c.Name)
			fmt.Printf("Size:       %d bytes\n", c.SizeBytes)
			fmt.Printf("Created:    %s\n", t)
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "rm [NAME]",
		Short: "Remove a config",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			_, err = client.RemoveConfig(ctx, args[0])
			if err != nil {
				return fmt.Errorf("remove config: %w", err)
			}
			fmt.Printf("✓ config %q removed\n", args[0])
			return nil
		},
	})

	return cmd
}
