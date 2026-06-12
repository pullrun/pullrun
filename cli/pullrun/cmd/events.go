// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"io"
	"os"
	"os/signal"
	"sort"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// NewEventsCommand implements `pullrun events [--follow] [--types=...]`
// — streams runtime events from the in-process broadcast bus.
//
// The runtime emits events for image pulls, image dedups, workload
// starts/stops/exits, backend selection, and policy decisions. The
// CLI subscribes via the gRPC `StreamEvents` RPC; the runtime
// forwards events from its `tokio::sync::broadcast` channel to the
// stream, applying any client-side filter on `event_types`.
func NewEventsCommand(opts *RootOptions) *cobra.Command {
	var (
		follow    bool
		eventTypes []string
		since     string
	)

	cmd := &cobra.Command{
		Use:   "events",
		Short: "Stream runtime events (pulls, runs, exits, policy decisions)",
		Args:  cobra.NoArgs,
		Long: `Stream events from the runtime. By default prints the live tail of events
emitted while the command is connected. With --follow (default), the
stream stays open until interrupted; without it, the command exits
as soon as the gRPC server closes the stream (which it does after one
batch in v0).

Event kinds: IMAGE_PULLED, IMAGE_DEDUPED, WORKLOAD_STARTED,
WORKLOAD_STOPPED, WORKLOAD_EXITED, BACKEND_SELECTED, POLICY_DENIED,
POLICY_ALLOWED. Filter with --types=KIND1,KIND2.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			// Forward SIGINT/SIGTERM into the gRPC stream's
			// context, so a Ctrl-C cleanly tears down both the
			// stream and the client. The runtime side will then
			// observe a Lagged and close the connection.
			sigCh := make(chan os.Signal, 1)
			signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
			go func() {
				<-sigCh
				cancel()
			}()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			// Normalize the event type filter to upper-case so the
			// CLI accepts lowercase input. Unknown kinds are
			// silently dropped by the runtime's filter, which is
			// the right behaviour (forward-compat: a future CLI
			// asking for a new kind via an old runtime gets an
			// empty stream rather than a hard error).
			var types []string
			for _, t := range eventTypes {
				if t = strings.ToUpper(strings.TrimSpace(t)); t != "" {
					types = append(types, t)
				}
			}
			if since != "" {
				// v0 doesn't support since; warn once on stderr.
				fmt.Fprintf(os.Stderr, "warning: --since is not yet implemented in v0; ignoring\n")
			}

			stream, err := client.StreamEvents(ctx, types)
			if err != nil {
				return fmt.Errorf("stream events: %w", err)
			}

			for {
				ev, err := stream.Recv()
				if err == io.EOF {
					return nil
				}
				if err != nil {
					// Context cancellation (Ctrl-C) shows
					// up as a context.Canceled error from
					// the gRPC stream. Don't print it as
					// a hard error.
					if ctx.Err() != nil {
						return nil
					}
					return fmt.Errorf("recv event: %w", err)
				}
				printEvent(ev)
			}
		},
	}
	cmd.Flags().BoolVarP(&follow, "follow", "f", true, "Keep the stream open and print events as they arrive (default true)")
	cmd.Flags().StringSliceVar(&eventTypes, "types", nil, "Filter to a comma-separated list of event kinds (e.g. WORKLOAD_EXITED,POLICY_DENIED)")
	cmd.Flags().StringVar(&since, "since", "", "Show events newer than a duration (e.g. 5m) — NOT YET IMPLEMENTED")
	return cmd
}

// printEvent formats a single event as a one-line record. The
// metadata is sorted by key for stable output.
func printEvent(ev *runtimepb.Event) {
	ts := time.Unix(ev.Timestamp, 0).Format(time.RFC3339)
	fmt.Printf("%s  %-20s  %s\n", ts, ev.Kind, ev.Id)
	if len(ev.Metadata) > 0 {
		keys := make([]string, 0, len(ev.Metadata))
		for k := range ev.Metadata {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		parts := make([]string, 0, len(keys))
		for _, k := range keys {
			parts = append(parts, fmt.Sprintf("%s=%s", k, ev.Metadata[k]))
		}
		fmt.Printf("                                       %s\n", strings.Join(parts, " "))
	}
}
