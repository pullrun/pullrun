package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

func newDownCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "down",
		Short: "Stop and remove services",
		Long:  `Stop all running services that were started by this compose project.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			filePath := resolveComposeFile()
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				return fmt.Errorf("compose file not found: %s", filePath)
			}

			project, err := parseComposeYAML(filePath)
			if err != nil {
				return fmt.Errorf("parse compose: %w", err)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
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

			for _, name := range names {
				id := fmt.Sprintf("%s-%s", project.Name, name)
				fmt.Printf("  stopping %s... ", name)

				_, err := client.StopWorkload(ctx, &runtimeapi.StopRequest{Id: id})
				if err != nil {
					fmt.Fprintf(os.Stderr, "FAILED: %v\n", err)
				} else {
					fmt.Println("OK")
				}
			}

			return nil
		},
	}
}
