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

// NewSnapshotCommand manages Firecracker VM memory snapshots.
func NewSnapshotCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "snapshot",
		Short: "Create and manage VM memory snapshots",
		Long: `Capture a running Firecracker VM's memory and disk state, or list and
delete existing snapshots. A snapshot is a terminal state for the source VM:
the VM is stopped after the snapshot is written.`,
	}

	create := &cobra.Command{
		Use:   "create WORKLOAD_ID",
		Short: "Snapshot a running VM and stop it",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.CreateSnapshot(context.Background(), args[0])
			if err != nil {
				return err
			}
			fmt.Printf("Snapshot created: %s\n", resp.SnapshotId)
			fmt.Printf("  workload:  %s\n", resp.WorkloadId)
			fmt.Printf("  memory:    %.1f MiB\n", float64(resp.MemBytes)/(1024*1024))
			fmt.Printf("  state:     %s\n", resp.StatePath)
			fmt.Printf("  rootfs:    %s\n", resp.RootfsPath)
			return nil
		},
	}

	ls := &cobra.Command{
		Use:   "ls",
		Short: "List snapshots",
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.ListSnapshots(context.Background())
			if err != nil {
				return err
			}
			w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
			fmt.Fprintln(w, "SNAPSHOT\tWORKLOAD\tMEMORY\tCREATED")
			for _, s := range resp.Snapshots {
				fmt.Fprintf(w, "%s\t%s\t%.1f MiB\t%d\n", s.SnapshotId, s.WorkloadId, float64(s.MemBytes)/(1024*1024), s.CreatedAt)
			}
			return w.Flush()
		},
	}

	rm := &cobra.Command{
		Use:   "rm SNAPSHOT_ID",
		Short: "Delete a snapshot",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.RemoveSnapshot(context.Background(), args[0])
			if err != nil {
				return err
			}
			if !resp.Removed {
				return fmt.Errorf("snapshot %s not found", args[0])
			}
			fmt.Printf("Removed %s\n", args[0])
			return nil
		},
	}

	cmd.AddCommand(create, ls, rm)
	return cmd
}
