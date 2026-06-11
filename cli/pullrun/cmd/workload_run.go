package cmd

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/spf13/cobra"
)

// NewWorkloadRunCommand implements `pullrun workload run`.
//
// Unlike `pullrun run` (which starts a workload and exits),
// this command opens a long-lived `AttachWorkload` gRPC bidi
// stream, sends the workload command/env/cwd, and pumps:
//
//   - the local terminal's stdin  → AttachStdin / AttachStdinEof
//   - the runtime's AttachStdout → the local terminal's stdout
//   - the runtime's AttachStderr → the local terminal's stderr
//   - the runtime's AttachExit   → exit code, propagate to os.Exit
//
// This is the equivalent of `docker attach` / `kubectl attach`.
// It reuses attachToWorkload which is also shared by `pullrun run --attach`.
func NewWorkloadRunCommand(opts *RootOptions) *cobra.Command {
	var (
		command    []string
		envVars    []string
		envMap     = map[string]string{}
		workingDir string
		backend    string
		tty        bool
	)

	cmd := &cobra.Command{
		Use:   "run [WORKLOAD_ID]",
		Short: "Attach to a running workload and stream its I/O",
		Long: `Open a bidirectional I/O stream to a running workload and
proxy your terminal's stdio through it. The first argument is
the workload ID (as returned by 'pullrun run' or 'pullrun list').

While the stream is open:
  - your terminal's stdin is sent to the workload as AttachStdin;
    EOF on stdin sends AttachStdinEof;
  - the workload's stdout comes back as AttachStdout, printed
    to your stdout;
  - the workload's stderr comes back as AttachStderr, printed
    to your stderr;
  - when the workload exits, the AttachExit frame's code is
    propagated as this command's exit code.

This is the equivalent of 'docker attach' / 'kubectl attach'.
Unlike 'pullrun run' (which spawns and returns), 'workload run'
blocks until the workload exits or the user detaches with ^C.`,
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			workloadID := args[0]

			var err error
			envMap, err = parseEnvVars(envVars)
			if err != nil {
				return err
			}

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			sigCh := make(chan os.Signal, 1)
			signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
			go func() {
				<-sigCh
				fmt.Fprintln(os.Stderr, "\n[pullrun] detached")
				cancel()
			}()

			_ = backend

			return attachToWorkload(ctx, client, workloadID, command, envMap, workingDir, tty)
		},
	}

	cmd.Flags().StringSliceVarP(&command, "cmd", "c", nil, "Command to run inside the workload (overrides entrypoint)")
	cmd.Flags().StringSliceVarP(&envVars, "env", "e", nil, "Environment variables (KEY=VALUE)")
	cmd.Flags().StringVar(&workingDir, "workdir", "/", "Working directory inside the workload")
	cmd.Flags().StringVar(&backend, "backend", "", "Backend hint (defaults to workload's runtime backend)")
	cmd.Flags().BoolVarP(&tty, "tty", "t", false, "Allocate a pseudo-TTY for the workload")
	return cmd
}

// NewWorkloadCommand returns the parent `pullrun workload` command.
func NewWorkloadCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "workload",
		Short: "Workload operations (run, exec, list, ...)",
	}
	cmd.AddCommand(NewWorkloadRunCommand(opts))
	return cmd
}
