// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/spf13/cobra"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

func newPsCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "ps",
		Short: "List service status",
		Long:  `Show the status of all services defined in the compose file.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			filePath := resolveComposeFile()
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				return fmt.Errorf("compose file not found: %s", filePath)
			}

			project, err := parseComposeYAML(filePath)
			if err != nil {
				return fmt.Errorf("parse compose: %w", err)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, conn, err := connectRuntime(ctx)
			if err != nil {
				fmt.Fprintf(os.Stderr, "Warning: cannot connect to runtime: %v\n", err)
				// Still show service names even without runtime
				for _, name := range project.ServiceNames() {
					fmt.Printf("%-30s %s\n", name, "unknown (no connection)")
				}
				return nil
			}
			defer conn.Close()

			listResp, err := client.ListWorkloads(ctx, &runtimeapi.ListWorkloadsRequest{})
			if err != nil {
				return fmt.Errorf("list workloads: %w", err)
			}

			// Build map of id -> status
			statusMap := make(map[string]*runtimeapi.WorkloadStatus)
			for _, wl := range listResp.Workloads {
				statusMap[wl.Id] = wl
			}

			fmt.Printf("%-30s %-12s %s\n", "SERVICE", "STATUS", "IP")
			fmt.Println(strings.Repeat("-", 60))

			for _, name := range project.ServiceNames() {
				id := fmt.Sprintf("%s-%s", project.Name, name)
				if wl, ok := statusMap[id]; ok {
					fmt.Printf("%-30s %-12s %s\n", name, wl.State, wl.InternalIp)
				} else {
					fmt.Printf("%-30s %-12s\n", name, "not found")
				}
			}

			return nil
		},
	}
}
