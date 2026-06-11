package cmd

import (
	"context"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/spf13/cobra"
)

func NewSecretCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "secret",
		Short: "Manage secrets (docker secret equivalent)",
	}

	cmd.AddCommand(&cobra.Command{
		Use:   "create [NAME] [FILE|-]",
		Short: "Create a secret from a file or stdin",
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

			_, err = client.CreateSecret(ctx, name, data)
			if err != nil {
				return fmt.Errorf("create secret: %w", err)
			}
			fmt.Printf("✓ secret %q created (%d bytes)\n", name, len(data))
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "ls",
		Short: "List secrets",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.ListSecrets(ctx)
			if err != nil {
				return fmt.Errorf("list secrets: %w", err)
			}
			if len(resp.Secrets) == 0 {
				fmt.Println("No secrets.")
				return nil
			}
			fmt.Printf("%-24s %10s  %s\n", "NAME", "SIZE", "CREATED")
			for _, s := range resp.Secrets {
				t := time.Unix(s.CreatedAt, 0).Format(time.RFC3339)
				fmt.Printf("%-24s %10d  %s\n", s.Name, s.SizeBytes, t)
			}
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "inspect [NAME]",
		Short: "Inspect a secret",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.InspectSecret(ctx, args[0])
			if err != nil {
				return fmt.Errorf("inspect secret: %w", err)
			}
			s := resp.Secret
			if s == nil {
				fmt.Printf("Secret %q not found.\n", args[0])
				return nil
			}
			t := time.Unix(s.CreatedAt, 0).Format(time.RFC3339)
			fmt.Printf("Name:       %s\n", s.Name)
			fmt.Printf("Size:       %d bytes\n", s.SizeBytes)
			fmt.Printf("Created:    %s\n", t)
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "rm [NAME]",
		Short: "Remove a secret",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			_, err = client.RemoveSecret(ctx, args[0])
			if err != nil {
				return fmt.Errorf("remove secret: %w", err)
			}
			fmt.Printf("✓ secret %q removed\n", args[0])
			return nil
		},
	})

	return cmd
}
