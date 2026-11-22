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

// NewActorCommand manages stateful actors backed by golden snapshots.
func NewActorCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "actor",
		Short: "Stateful actors (suspend/resume from golden snapshots)",
	}

	var template string
	create := &cobra.Command{
		Use:   "create NAME",
		Short: "Create a suspended actor from a template",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			if template == "" {
				return fmt.Errorf("--template is required")
			}
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.CreateActor(context.Background(), args[0], template)
			if err != nil {
				return err
			}
			fmt.Printf("Actor %s created (%s)\n", resp.Name, resp.State)
			return nil
		},
	}
	create.Flags().StringVar(&template, "template", "", "Template to instantiate")

	ls := &cobra.Command{
		Use:   "ls",
		Short: "List actors",
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.ListActors(context.Background())
			if err != nil {
				return err
			}
			w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
			fmt.Fprintln(w, "ACTOR\tTEMPLATE\tSTATE\tWORKLOAD")
			for _, a := range resp.Actors {
				fmt.Fprintf(w, "%s\t%s\t%s\t%s\n", a.Name, a.Template, a.State, a.WorkloadId)
			}
			return w.Flush()
		},
	}

	resume := &cobra.Command{
		Use:   "resume NAME",
		Short: "Resume a suspended actor",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.ResumeActor(context.Background(), args[0])
			if err != nil {
				return err
			}
			fmt.Printf("Actor %s running as %s (resumed=%v, %d ms)\n", args[0], resp.WorkloadId, resp.Resumed, resp.ResumeMs)
			return nil
		},
	}

	suspend := &cobra.Command{
		Use:   "suspend NAME",
		Short: "Snapshot a running actor and stop it",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.SuspendActor(context.Background(), args[0])
			if err != nil {
				return err
			}
			fmt.Printf("Actor %s suspended (%s)\n", args[0], resp.SnapshotId)
			return nil
		},
	}

	run := &cobra.Command{
		Use:   "run NAME",
		Short: "Resume an actor and attach to its console",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.ResumeActor(context.Background(), args[0])
			if err != nil {
				return err
			}
			fmt.Fprintf(os.Stderr, "[actor] %s running as %s (%d ms)\n", args[0], resp.WorkloadId, resp.ResumeMs)
			return attachToWorkload(context.Background(), client, resp.WorkloadId, nil, nil, "", false)
		},
	}

	rm := &cobra.Command{
		Use:   "rm NAME",
		Short: "Delete an actor",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()
			resp, err := client.RemoveActor(context.Background(), args[0])
			if err != nil {
				return err
			}
			if !resp.Removed {
				return fmt.Errorf("actor %s not found", args[0])
			}
			fmt.Printf("Removed %s\n", args[0])
			return nil
		},
	}

	cmd.AddCommand(create, ls, resume, suspend, run, rm)
	return cmd
}
