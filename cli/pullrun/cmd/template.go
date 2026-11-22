// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"os"
	"text/tabwriter"

	"github.com/spf13/cobra"
)

// NewTemplateCommand manages golden actor templates.
func NewTemplateCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "template",
		Short: "Golden actor templates (memory snapshots)",
	}

	var fromWorkload string
	create := &cobra.Command{
		Use:   "create NAME",
		Short: "Snapshot a running workload into a golden template",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			if fromWorkload == "" {
				return fmt.Errorf("--from-workload is required")
			}
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.CreateTemplate(context.Background(), args[0], fromWorkload)
			if err != nil {
				return err
			}
			fmt.Printf("Template %s created from snapshot %s\n", resp.Name, resp.SnapshotId)
			return nil
		},
	}
	create.Flags().StringVar(&fromWorkload, "from-workload", "", "Running workload to snapshot")

	ls := &cobra.Command{
		Use:   "ls",
		Short: "List templates",
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.ListTemplates(context.Background())
			if err != nil {
				return err
			}
			w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
			fmt.Fprintln(w, "TEMPLATE\tSNAPSHOT\tIMAGE\tCOMMAND")
			for _, t := range resp.Templates {
				fmt.Fprintf(w, "%s\t%s\t%s\t%s\n", t.Name, t.SnapshotId, t.ImageRoot, joinCommand(t.Command))
			}
			return w.Flush()
		},
	}

	rm := &cobra.Command{
		Use:   "rm NAME",
		Short: "Delete a template",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.RemoveTemplate(context.Background(), args[0])
			if err != nil {
				return err
			}
			if !resp.Removed {
				return fmt.Errorf("template %s not found", args[0])
			}
			fmt.Printf("Removed %s\n", args[0])
			return nil
		},
	}

	cmd.AddCommand(create, ls, rm)
	return cmd
}

func joinCommand(args []string) string {
	out := ""
	for i, a := range args {
		if i > 0 {
			out += " "
		}
		out += a
	}
	return out
}
