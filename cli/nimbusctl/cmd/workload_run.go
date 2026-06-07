package cmd

import (
	"context"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strings"
	"syscall"

	"github.com/spf13/cobra"
	runtimepb "nimbus/protoapi/nimbus/runtime"
)

// NewWorkloadRunCommand implements `nimbusctl workload run`.
//
// Unlike `nimbusctl run` (which starts a workload and exits),
// this command opens a long-lived `AttachWorkload` gRPC bidi
// stream, sends the workload command/env/cwd, and pumps:
//
//   - the local terminal's stdin  → AttachStdin / AttachStdinEof
//   - the runtime's AttachStdout → the local terminal's stdout
//   - the runtime's AttachStderr → the local terminal's stderr
//   - the runtime's AttachExit   → exit code, propagate to os.Exit
//
// This is the equivalent of `docker run -it` or
// `kubectl exec -it`. It requires a TTY-aware workload
// backend (the `vm` backend with the Apple Virt FFI), so for
// v0 it returns an error from the runtime if the backend is
// not available. The `container` backend does NOT support
// attach in v0.
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
the workload ID (as returned by 'nimbusctl run' or 'nimbusctl list').

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
Unlike 'nimbusctl run' (which spawns and returns), 'workload run'
blocks until the workload exits or the user detaches with ^C.`,
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			workloadID := args[0]

			// Parse env vars
			for _, e := range envVars {
				parts := strings.SplitN(e, "=", 2)
				if len(parts) != 2 {
					return fmt.Errorf("invalid env var %q (expected KEY=VALUE)", e)
				}
				envMap[parts[0]] = parts[1]
			}

			// Ensure the runtime is running and get a client.
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			// Handle SIGINT / SIGTERM by cancelling the
			// context, which closes the gRPC stream and
			// lets the runtime's attach path tear down
			// the workload's stdin pipe.
			sigCh := make(chan os.Signal, 1)
			signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
			go func() {
				<-sigCh
				fmt.Fprintln(os.Stderr, "\n[nimbus] detached")
				cancel()
			}()

			// Open the bidi stream.
			stream, err := client.AttachWorkload(ctx)
			if err != nil {
				return fmt.Errorf("attach: %w", err)
			}

			// 1. Send the open message.
			if err := stream.Send(&runtimepb.AttachMessage{
				Body: &runtimepb.AttachMessage_Open{
					Open: &runtimepb.AttachOpen{
						WorkloadId: workloadID,
						Command:    command,
						Env:        envMap,
						WorkingDir: workingDir,
					},
				},
			}); err != nil {
				return fmt.Errorf("send open: %w", err)
			}

			_ = tty   // accepted for forward-compat; v0 doesn't translate LF↔CRLF
			_ = backend // accepted for forward-compat; v0 uses whatever backend the workload was started with

			// 2. Pump stdin (host → workload).
			stdinDone := make(chan struct{})
			go func() {
				defer close(stdinDone)
				buf := make([]byte, 4096)
				for {
					n, err := os.Stdin.Read(buf)
					if n > 0 {
						chunk := make([]byte, n)
						copy(chunk, buf[:n])
						if sendErr := stream.Send(&runtimepb.AttachMessage{
							Body: &runtimepb.AttachMessage_Stdin{
								Stdin: &runtimepb.AttachStdin{Data: chunk},
							},
						}); sendErr != nil {
							return
						}
					}
					if err == io.EOF {
						_ = stream.Send(&runtimepb.AttachMessage{
							Body: &runtimepb.AttachMessage_StdinEof{
								StdinEof: &runtimepb.AttachStdinEof{},
							},
						})
						return
					}
					if err != nil {
						return
					}
				}
			}()

			// 3. Pump stdout/stderr/exit (workload → host).
			exitCode := 0
			for {
				msg, err := stream.Recv()
				if err == io.EOF {
					break
				}
				if err != nil {
					// If the context was cancelled (user
					// pressed ^C), don't report a noisy
					// error; the deferred cancel() will
					// take care of cleanup.
					if ctx.Err() != nil {
						break
					}
					return fmt.Errorf("recv: %w", err)
				}
				switch body := msg.Body.(type) {
				case *runtimepb.AttachMessage_Stdout:
					if body.Stdout != nil {
						if _, err := os.Stdout.Write(body.Stdout.Data); err != nil {
							return fmt.Errorf("write stdout: %w", err)
						}
					}
				case *runtimepb.AttachMessage_Stderr:
					if body.Stderr != nil {
						if _, err := os.Stderr.Write(body.Stderr.Data); err != nil {
							return fmt.Errorf("write stderr: %w", err)
						}
					}
				case *runtimepb.AttachMessage_Exit:
					if body.Exit != nil && body.Exit.HasExitCode {
						exitCode = int(body.Exit.ExitCode)
					}
				case *runtimepb.AttachMessage_Error:
					if body.Error != nil {
						fmt.Fprintf(os.Stderr, "[nimbus] runtime error: %s\n", body.Error.Message)
					}
				}
			}

			// Wait for the stdin pump to exit (it will
			// when the stream is closed).
			<-stdinDone

			// Propagate the workload's exit code.
			if exitCode != 0 {
				os.Exit(exitCode)
			}
			return nil
		},
	}

	cmd.Flags().StringSliceVarP(&command, "cmd", "c", nil, "Command to run inside the workload (overrides entrypoint)")
	cmd.Flags().StringSliceVarP(&envVars, "env", "e", nil, "Environment variables (KEY=VALUE)")
	cmd.Flags().StringVar(&workingDir, "workdir", "/", "Working directory inside the workload")
	cmd.Flags().StringVar(&backend, "backend", "", "Backend hint (defaults to workload's runtime backend)")
	cmd.Flags().BoolVarP(&tty, "tty", "t", false, "Allocate a pseudo-TTY (v0: accepted, ignored)")
	return cmd
}

// NewWorkloadCommand returns the parent `nimbusctl workload` command.
func NewWorkloadCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "workload",
		Short: "Workload operations (run, exec, list, ...)",
	}
	cmd.AddCommand(NewWorkloadRunCommand(opts))
	return cmd
}
