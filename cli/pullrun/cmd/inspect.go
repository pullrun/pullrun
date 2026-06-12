// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// NewInspectCommand implements `pullrun inspect [ID]` — returns a
// deep snapshot of a workload, including its DAG path (manifest →
// tree → layers/blobs) and the policy decision log.
func NewInspectCommand(opts *RootOptions) *cobra.Command {
	var asJSON bool

	cmd := &cobra.Command{
		Use:   "inspect [ID]",
		Short: "Deep inspection of a workload: state, DAG, network rules, policy decisions",
		Args:  cobra.ExactArgs(1),
		Long: `Inspect a workload. The DAG path is the ordered list of digest:kind nodes
from the root manifest down to the leaf blobs, so operators can see the
image's exact composition at a glance. The policy decision log records
which policies allowed or denied the workload, with reasons.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.InspectWorkload(ctx, args[0])
			if err != nil {
				return fmt.Errorf("inspect %s: %w", args[0], err)
			}

			if !resp.Found {
				return fmt.Errorf("workload %s not found (GC'd or never existed)", args[0])
			}

			if asJSON {
				enc := json.NewEncoder(os.Stdout)
				enc.SetIndent("", "  ")
				return enc.Encode(resp)
			}
			printInspectHuman(resp)
			return nil
		},
	}
	cmd.Flags().BoolVar(&asJSON, "json", false, "Output as JSON")
	return cmd
}

func printInspectHuman(r *runtimepb.InspectResponse) {
	fmt.Printf("ID:          %s\n", r.Id)
	fmt.Printf("State:       %s\n", r.State)
	fmt.Printf("Backend:     %s\n", r.Backend)
	if r.ImageRoot != "" {
		fmt.Printf("Image Root:  %s\n", r.ImageRoot)
	}
	if r.InternalIp != "" {
		fmt.Printf("IP:          %s\n", r.InternalIp)
	}
	if r.Pid > 0 {
		fmt.Printf("PID:         %d\n", r.Pid)
	}
	if r.StartTime > 0 {
		fmt.Printf("Started:     %s\n", time.Unix(r.StartTime, 0).Format(time.RFC3339))
	}
	if r.ExitTime > 0 {
		fmt.Printf("Exited:      %s\n", time.Unix(r.ExitTime, 0).Format(time.RFC3339))
	}
	if r.ExitCode > 0 || r.State == "exited" || r.State == "stopped" {
		fmt.Printf("Exit Code:   %d\n", r.ExitCode)
	}
	if len(r.Command) > 0 {
		fmt.Printf("Command:     %s\n", strings.Join(r.Command, " "))
	}
	if len(r.NetworkRules) > 0 {
		fmt.Println()
		fmt.Println("Network Rules:")
		for _, rule := range r.NetworkRules {
			host := rule.ToHost
			if host == "" {
				host = "*"
			}
			cidrs := strings.Join(rule.FromCidrs, ",")
			if cidrs == "" {
				cidrs = "*"
			}
			fmt.Printf("  %s/%s port=%d to=%s from=%s\n",
				rule.Direction, rule.Protocol, rule.Port, host, cidrs)
		}
	}
	if len(r.DagPath) > 0 {
		fmt.Println()
		fmt.Println("DAG Path:")
		for _, n := range r.DagPath {
			short := n.Digest
			// Truncate the digest for readability. Show the
			// algorithm prefix and first 12 hex chars.
			if i := strings.IndexByte(short, ':'); i >= 0 {
				short = short[:i+13]
				if len(short) < len(n.Digest) {
					short += "…"
				}
			}
			fmt.Printf("  %-12s %s\n", n.Kind, short)
		}
	}
	if len(r.PolicyDecisions) > 0 {
		fmt.Println()
		fmt.Println("Policy Decisions:")
		// Sort the keys so the output is stable across runs.
		keys := make([]string, 0, len(r.PolicyDecisions))
		for k := range r.PolicyDecisions {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			fmt.Printf("  %s: %s\n", k, r.PolicyDecisions[k])
		}
	}
}
