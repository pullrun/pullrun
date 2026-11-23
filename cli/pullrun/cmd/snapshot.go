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

	var fidelity string
	var parentID string
	create := &cobra.Command{
		Use:   "create WORKLOAD_ID",
		Short: "Snapshot a running VM (memory or rootfs)",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.CreateSnapshotWithParent(context.Background(), args[0], fidelity, parentID)
			if err != nil {
				return err
			}
			fmt.Printf("Snapshot created: %s (%s)\n", resp.SnapshotId, resp.Fidelity)
			fmt.Printf("  workload:  %s\n", resp.WorkloadId)
			fmt.Printf("  memory:    %.1f MiB\n", float64(resp.MemBytes)/(1024*1024))
			fmt.Printf("  state:     %s\n", resp.StatePath)
			fmt.Printf("  rootfs:    %s\n", resp.RootfsPath)
			fmt.Printf("  capture:   %d ms\n", resp.SnapshotMs)
			if resp.PagesTotal > 0 {
				fmt.Printf("  pages:     %d total, %d new\n", resp.PagesTotal, resp.PagesNew)
			}
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
			fmt.Fprintln(w, "SNAPSHOT\tFIDELITY\tWORKLOAD\tMEMORY\tCREATED")
			for _, s := range resp.Snapshots {
				fmt.Fprintf(w, "%s\t%s\t%s\t%.1f MiB\t%d\n", s.SnapshotId, s.Fidelity, s.WorkloadId, float64(s.MemBytes)/(1024*1024), s.CreatedAt)
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

	var fork bool
	restore := &cobra.Command{
		Use:   "restore SNAPSHOT_ID",
		Short: "Resume a snapshot into a new workload",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.RestoreSnapshotWithFork(context.Background(), args[0], fork)
			if err != nil {
				return err
			}
			fmt.Printf("Restored %s as workload %s (backend %s, %d ms)\n", args[0], resp.WorkloadId, resp.Backend, resp.RestoreMs)
			return nil
		},
	}

	var keepLast uint32
	var dryRun bool
	gc := &cobra.Command{
		Use:   "gc",
		Short: "Delete unreferenced snapshots (referenced by actors/templates are kept)",
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.GcSnapshots(context.Background(), keepLast, dryRun)
			if err != nil {
				return err
			}
			for _, id := range resp.Removed {
				fmt.Printf("removed %s\n", id)
			}
			for _, id := range resp.Kept {
				fmt.Printf("kept    %s\n", id)
			}
			fmt.Printf("freed %.1f MiB\n", float64(resp.BytesFreed)/(1024*1024))
			return nil
		},
	}
	gc.Flags().Uint32Var(&keepLast, "keep", 0, "Keep the newest N unreferenced snapshots")
	gc.Flags().BoolVar(&dryRun, "dry-run", false, "Print candidates without deleting")

	create.Flags().StringVar(&fidelity, "fidelity", "memory", "memory (RAM + disk, stops the VM), rootfs (disk only), or volumes")
	create.Flags().StringVar(&parentID, "parent", "", "Parent snapshot ID (incremental bookkeeping)")
	restore.Flags().BoolVar(&fork, "fork", false, "Isolate the restored guest on its own slirp network namespace (unique TAP)")
	cmd.AddCommand(create, gc, ls, rm, restore)
	return cmd
}
