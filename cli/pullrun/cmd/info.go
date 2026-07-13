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

func NewInfoCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "info",
		Short: "Show runtime information",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			resp, err := client.RuntimeInfo(ctx, &runtimepb.InfoRequest{})
			if err != nil {
				return fmt.Errorf("info: %w", err)
			}

			fmt.Printf("Version:         %s\n", resp.Version)
			fmt.Printf("Uptime:          %ds\n", resp.UptimeSeconds)
			fmt.Printf("Workloads:       %d\n", resp.WorkloadCount)
			fmt.Printf("Store path:      %s\n", resp.StoreMountpoint)
			if resp.StoreTotalBytes > 0 {
				fmt.Printf("Store total:     %d bytes\n", resp.StoreTotalBytes)
				fmt.Printf("Store used:      %d bytes\n", resp.StoreUsedBytes)
			}
			fmt.Printf("Store nodes:     %d\n", resp.StoreTotalNodes)
			return nil
		},
	}
}

func NewVersionCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "version",
		Short: "Print the client version",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			fmt.Println("pullrun 0.6.4")
			return nil
		},
	}
}
