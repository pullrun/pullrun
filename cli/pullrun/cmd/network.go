package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "pullrun/protoapi/pullrun/runtime"

	"github.com/spf13/cobra"
)

func NewNetworkCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "network",
		Short: "Manage networks",
	}

	cmd.AddCommand(&cobra.Command{
		Use:   "create <name>",
		Short: "Create a user-defined bridge network",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			subnet, _ := cmd.Flags().GetString("subnet")
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.CreateNetwork(ctx, &runtimepb.CreateNetworkRequest{
				Name:   args[0],
				Subnet: subnet,
			})
			if err != nil {
				return fmt.Errorf("create network: %w", err)
			}
			fmt.Printf("Created network %s\n", resp.BridgeName)
			fmt.Printf("  Subnet: %s\n", resp.Subnet)
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "rm <name>",
		Short: "Remove a network",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			_, err = client.RemoveNetwork(ctx, &runtimepb.RemoveNetworkRequest{
				Name: args[0],
			})
			if err != nil {
				return fmt.Errorf("remove network: %w", err)
			}
			fmt.Printf("Removed network %s\n", args[0])
			return nil
		},
	})

	cmd.AddCommand(&cobra.Command{
		Use:   "ls",
		Short: "List networks",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.ListNetworks(ctx, &runtimepb.ListNetworksRequest{})
			if err != nil {
				return fmt.Errorf("list networks: %w", err)
			}
			if len(resp.Networks) == 0 {
				fmt.Println("No networks.")
				return nil
			}
			for _, n := range resp.Networks {
				fmt.Printf("%s  subnet=%s  workloads=%d\n", n.Name, n.Subnet, n.AttachedWorkloads)
			}
			return nil
		},
	})

	cmd.PersistentFlags().String("subnet", "", "Custom subnet (e.g. 10.43.1.0/24)")
	return cmd
}
