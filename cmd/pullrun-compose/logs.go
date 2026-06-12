// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/spf13/cobra"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

func newLogsCommand() *cobra.Command {
	var follow bool
	var tail int64

	cmd := &cobra.Command{
		Use:   "logs [service...]",
		Short: "Show logs from services",
		Long:  `Display log output from services. If no service is specified, show logs from all services.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			filePath := resolveComposeFile()
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				return fmt.Errorf("compose file not found: %s", filePath)
			}

			project, err := parseComposeYAML(filePath)
			if err != nil {
				return fmt.Errorf("parse compose: %w", err)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
			defer cancel()

			client, conn, err := connectRuntime(ctx)
			if err != nil {
				return fmt.Errorf("connect to pullrun runtime: %w", err)
			}
			defer conn.Close()

			names := project.ServiceNames()
			if len(args) > 0 {
				names = args
			}

			if follow {
				// For follow mode, we'd need to set up a streaming connection
				// per service. For v0, just show tail logs.
				fmt.Fprintf(os.Stderr, "Warning: follow mode not yet implemented; showing tail logs\n")
			}

			for _, name := range names {
				id := fmt.Sprintf("%s-%s", project.Name, name)

				stream, err := client.StreamLogs(ctx, &runtimeapi.StreamLogsRequest{
					Id:     id,
					Tail:   tail,
					Follow: false,
				})
				if err != nil {
					fmt.Fprintf(os.Stderr, "  %s: log error: %v\n", name, err)
					continue
				}

				for {
					chunk, err := stream.Recv()
					if err == io.EOF {
						break
					}
					if err != nil {
						break
					}
					prefix := name
					if chunk.Stderr {
						fmt.Fprintf(os.Stderr, "[%s] %s", prefix, string(chunk.Data))
					} else {
						fmt.Printf("[%s] %s", prefix, string(chunk.Data))
					}
				}
			}

			return nil
		},
	}

	cmd.Flags().BoolVar(&follow, "follow", false, "Follow log output (not yet implemented)")
	cmd.Flags().Int64VarP(&tail, "tail", "n", 50, "Number of lines to show from end")

	return cmd
}
