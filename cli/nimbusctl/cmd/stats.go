package cmd

import (
	"context"
	"fmt"
	"time"

	"github.com/spf13/cobra"
	runtimepb "nimbus/protoapi/nimbus/runtime"
)

func NewStatsCommand(opts *RootOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "stats <id>",
		Short: "Show live resource stats for a workload",
		Long: `Display current CPU and memory usage for a running workload.
Stats are read from cgroupfs via the runtime's GetWorkloadStats RPC.`,
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			id := args[0]
			resp, err := client.GetWorkloadStats(ctx, &runtimepb.GetWorkloadStatsRequest{Id: id})
			if err != nil {
				return fmt.Errorf("stats: %w", err)
			}

			fmt.Printf("Workload: %s\n", resp.Id)
			fmt.Printf("  CPU:         %.1f%%\n", resp.CpuUsagePercent)
			fmt.Printf("  Memory:      %d bytes (%.1f MB)\n", resp.MemoryBytes, float64(resp.MemoryBytes)/1048576)
			fmt.Printf("  Disk:        %d bytes\n", resp.DiskBytes)
			fmt.Printf("  Network RX:  %d bytes\n", resp.NetworkRxBytes)
			fmt.Printf("  Network TX:  %d bytes\n", resp.NetworkTxBytes)
			return nil
		},
	}
}
