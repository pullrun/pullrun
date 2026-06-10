package cmd

import (
	"context"
	"fmt"
	"time"

	"github.com/spf13/cobra"
	runtimepb "nimbus/protoapi/nimbus/runtime"
)

func NewUpdateCommand(opts *RootOptions) *cobra.Command {
	var cpuMillicores uint64
	var memoryBytes uint64

	cmd := &cobra.Command{
		Use:   "update <id>",
		Short: "Update container resource limits",
		Long: `Update resource limits for a running container.
Sets CPU and/or memory limits. Values of 0 are treated as "no change".
Examples:
  nimbusctl update my-container --cpu 2000
  nimbusctl update my-container --memory 536870912
  nimbusctl update my-container --cpu 1000 --memory 268435456
`,
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, cleanup, err := ensureGRPCClient(opts)
			if err != nil {
				return fmt.Errorf("connect: %w", err)
			}
			defer cleanup()

			req := &runtimepb.UpdateWorkloadRequest{
				Id:            args[0],
				CpuMillicores: cpuMillicores,
				MemoryBytes:   memoryBytes,
			}

			resp, err := client.UpdateWorkload(ctx, req)
			if err != nil {
				return fmt.Errorf("update: %w", err)
			}

			if resp.Success {
				fmt.Println("Resources updated.")
			} else {
				return fmt.Errorf("update failed or no changes applied")
			}
			return nil
		},
	}

	cmd.Flags().Uint64Var(&cpuMillicores, "cpu", 0, "CPU millicores (1000 = 1 vCPU); 0 = no change")
	cmd.Flags().Uint64Var(&memoryBytes, "memory", 0, "Memory limit in bytes; 0 = no change")

	return cmd
}
