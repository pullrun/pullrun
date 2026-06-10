package cmd

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	runtimepb "nimbus/protoapi/nimbus/runtime"
)

// ensureGRPCClient returns a connected gRPC client. If direct mode is enabled
// and the runtime is not running, it spawns nimbus-runtime as a child process.
func ensureGRPCClient(opts *RootOptions) (*GRPCClient, func(), error) {
	if opts.ServerAddr != "" {
		return nil, nil, fmt.Errorf("control-plane path not yet implemented; use --direct mode")
	}

	// Spawn runtime if needed (direct mode)
	if opts.DirectMode {
		if _, err := os.Stat(opts.SocketPath); os.IsNotExist(err) {
			if err := spawnRuntime(opts); err != nil {
				return nil, nil, fmt.Errorf("spawn runtime: %w", err)
			}
		}
	}

	client, err := NewGRPCClient(opts.SocketPath)
	if err != nil {
		return nil, nil, err
	}
	return client, func() { client.Close() }, nil
}

// generateWorkloadID returns a short unique identifier (12 hex chars).
func generateWorkloadID() string {
	var b [6]byte
	_, _ = rand.Read(b[:])
	return "wl-" + hex.EncodeToString(b[:])
}

// parseEnvVars parses KEY=VALUE pairs from a string slice into a map.
func parseEnvVars(envVars []string) (map[string]string, error) {
	envMap := make(map[string]string, len(envVars))
	for _, e := range envVars {
		parts := strings.SplitN(e, "=", 2)
		if len(parts) != 2 {
			return nil, fmt.Errorf("invalid env var %q (expected KEY=VALUE)", e)
		}
		envMap[parts[0]] = parts[1]
	}
	return envMap, nil
}

func NewPullCommand(opts *RootOptions) *cobra.Command {
	var (
		registry string
		platform string
	)

	cmd := &cobra.Command{
		Use:   "pull [IMAGE]",
		Short: "Pull an OCI image and store it in the DAG",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			imageRef := args[0]
			auth, _ := GetRegistryAuth(NormalizeRegistry(registry))
			resp, err := client.PullImage(ctx, imageRef, registry, platform, auth)
			if err != nil {
				return fmt.Errorf("pull %s: %w", imageRef, err)
			}

			fmt.Printf("✓ %s\n", imageRef)
			fmt.Printf("  root digest: %s\n", resp.RootDigest)
			fmt.Printf("  stored:      %d bytes\n", resp.BytesStored)
			if resp.BytesDeduplicated > 0 {
				fmt.Printf("  deduped:     %d bytes\n", resp.BytesDeduplicated)
			}
			if platform != "" {
				fmt.Printf("  platform:    %s\n", platform)
			}
			return nil
		},
	}
	cmd.Flags().StringVar(&registry, "registry", "docker.io", "Registry to pull from")
	cmd.Flags().StringVar(&platform, "platform", "", "Target platform (e.g. linux/amd64, linux/arm64)")
	return cmd
}

func parseVolumeSpec(spec string) (*runtimepb.Mount, error) {
	// Format: source:destination[:options]
	parts := strings.SplitN(spec, ":", 3)
	if len(parts) < 2 {
		return nil, fmt.Errorf("invalid volume spec %q: expected source:destination[:options]", spec)
	}
	m := &runtimepb.Mount{
		Type:        "bind",
		Source:      parts[0],
		Destination: parts[1],
	}
	if len(parts) == 3 {
		m.Options = strings.Split(parts[2], ",")
	}
	return m, nil
}

func NewRunCommand(opts *RootOptions) *cobra.Command {
	var (
		backend         string
		allowOutbound   []string
		allowInbound    []string
		publishPorts    []string
		envVars         []string
		envMap          = map[string]string{}
		command         []string
		cpuMillicores   uint64
		memoryBytes     uint64
		networkMode     string
		name            string
		kernelImage     string
		registry        string
		volumes         []string
		healthCmd       string
		healthInterval  uint32
		healthTimeout   uint32
		healthRetries   uint32
		healthStartPeriod uint32
		restartPolicy   string
		platform        string
		secretNames     []string
		configNames     []string
		attach          bool
	)

	cmd := &cobra.Command{
		Use:   "run [IMAGE_REF|DIGEST]",
		Short: "Run a workload from a DAG root or image reference",
		Args:  cobra.ExactArgs(1),
		Long: `Run a workload. Accepts either a content-addressed digest (sha256:...)
or an image:tag reference (e.g. alpine:latest) which will be pulled first.

Use --backend=vm --kernel-image=<ref> to run inside an Apple Virt micro-VM
(macOS only). The kernel is an OCI image and will be staged from the local
DAG store if not already present.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			rootDigest := args[0]

			// If not a digest, pull first
			if !strings.HasPrefix(rootDigest, "sha256:") {
				pullRegistry := registry
				if pullRegistry == "" {
					pullRegistry = "docker.io"
				}
				auth, _ := GetRegistryAuth(NormalizeRegistry(pullRegistry))
				pullResp, err := client.PullImage(ctx, rootDigest, pullRegistry, platform, auth)
				if err != nil {
					return fmt.Errorf("pull %s: %w", rootDigest, err)
				}
				rootDigest = pullResp.RootDigest
				fmt.Fprintf(os.Stderr, "pulled %s -> %s\n", args[0], rootDigest)
			}

			// Build network rules
			rules, err := buildNetworkRules(allowOutbound, allowInbound, publishPorts)
			if err != nil {
				return err
			}

			// Parse env vars
			envMap, err = parseEnvVars(envVars)
			if err != nil {
				return err
			}

			id := name
			if id == "" {
				id = generateWorkloadID()
			}

			// Parse volume specs
			var mounts []*runtimepb.Mount
			for _, v := range volumes {
				m, err := parseVolumeSpec(v)
				if err != nil {
					return err
				}
				mounts = append(mounts, m)
			}

			// Health check configuration
			var healthCheck *runtimepb.HealthCheck
			if healthCmd != "" {
				healthCheck = &runtimepb.HealthCheck{
					Test:               []string{"CMD-SHELL", healthCmd},
					IntervalSeconds:    healthInterval,
					TimeoutSeconds:     healthTimeout,
					Retries:            healthRetries,
					StartPeriodSeconds: healthStartPeriod,
				}
				if healthCheck.IntervalSeconds == 0 {
					healthCheck.IntervalSeconds = 30
				}
				if healthCheck.TimeoutSeconds == 0 {
					healthCheck.TimeoutSeconds = 30
				}
				if healthCheck.Retries == 0 {
					healthCheck.Retries = 3
				}
			}

			// Parse --secret and --config references
			var secretRefs []*runtimepb.SecretRef
			for _, s := range secretNames {
				parts := strings.SplitN(s, "=", 2)
				ref := &runtimepb.SecretRef{Name: parts[0]}
				if len(parts) == 2 {
					ref.TargetPath = parts[1]
				}
				secretRefs = append(secretRefs, ref)
			}
			var configRefs []*runtimepb.ConfigRef
			for _, c := range configNames {
				parts := strings.SplitN(c, "=", 2)
				ref := &runtimepb.ConfigRef{Name: parts[0]}
				if len(parts) == 2 {
					ref.TargetPath = parts[1]
				}
				configRefs = append(configRefs, ref)
			}

			restartProto, err := parseRestartPolicy(restartPolicy)
			if err != nil {
				return err
			}
			resp, err := client.RunWorkload(ctx, &runtimepb.RunRequest{
				Id:            id,
				RootDigest:    rootDigest,
				Backend:       backend,
				Command:       command,
				Env:           envMap,
				CpuMillicores: cpuMillicores,
				MemoryBytes:   memoryBytes,
				NetworkMode:   networkMode,
				NetworkRules:  rules,
				KernelImage:   kernelImage,
				Mounts:        mounts,
				HealthCheck:   healthCheck,
				RestartPolicy: restartProto,
				Secrets:       secretRefs,
				Configs:       configRefs,
			})
			if err != nil {
				return fmt.Errorf("run workload: %w", err)
			}

			// Print a one-line notice (compatible with --attach output).
			// When --attach is set, we skip the multi-line summary so the
			// workload's stdout appears immediately below.
			if !attach {
				fmt.Printf("Started %s\n", resp.Id)
				fmt.Printf("  backend:    %s\n", resp.BackendUsed)
				fmt.Printf("  pid:        %d\n", resp.Pid)
				if resp.InternalIp != "" {
					fmt.Printf("  internal:   %s\n", resp.InternalIp)
				}
			}

			if attach {
				// Switch to a background context — the workload is already
				// running and should keep running even if the 60s pull/run
				// context has less time left.
				attachCtx, attachCancel := context.WithCancel(context.Background())
				defer attachCancel()

				if backend == "vm" {
					return attachToWorkload(attachCtx, client, resp.Id, nil, nil, "")
				}
				// Container backend: stream logs and poll for exit code.
				return streamAndWait(attachCtx, client, resp.Id)
			}

			return nil
		},
	}

	cmd.Flags().StringVar(&backend, "backend", "container", "Backend: container, vm, sandbox")
	cmd.Flags().StringVar(&name, "name", "", "Workload name (auto-generated if empty)")
	cmd.Flags().StringSliceVar(&allowOutbound, "allow-outbound", nil, "Allow outbound: tcp:host:port")
	cmd.Flags().StringSliceVar(&allowInbound, "allow-inbound", nil, "Allow inbound port (e.g. 8080)")
	cmd.Flags().StringSliceVarP(&publishPorts, "publish", "p", nil, "Publish host:container port (e.g. 8080:80, or just 8080)")
	cmd.Flags().StringSliceVarP(&envVars, "env", "e", nil, "Environment variables (KEY=VALUE)")
	cmd.Flags().StringSliceVar(&command, "cmd", nil, "Override entrypoint command")
	cmd.Flags().Uint64Var(&cpuMillicores, "cpu", 1000, "CPU millicores (1000 = 1 vCPU)")
	cmd.Flags().Uint64Var(&memoryBytes, "memory", 512*1024*1024, "Memory limit in bytes")
	cmd.Flags().StringVar(&networkMode, "net", "isolated", "Network mode: isolated|host|none")
	cmd.Flags().StringVar(&kernelImage, "kernel-image", "", "OCI reference for the kernel image (required for --backend=vm, e.g. 'nimbus/kernel-asahi:6.19.14')")
	cmd.Flags().StringVar(&registry, "registry", "", "Registry to pull the workload image from (default: docker.io; use 'localhost:5000' for local registries)")
	cmd.Flags().StringSliceVarP(&volumes, "volume", "v", nil, "Bind mount (source:destination[:options]), e.g. /host/path:/container/path:ro")
	cmd.Flags().StringVar(&healthCmd, "health-cmd", "", "Health check command (e.g. 'curl -f http://localhost:80' or 'ls /tmp/healthy')")
	cmd.Flags().Uint32Var(&healthInterval, "health-interval", 30, "Health check interval (seconds)")
	cmd.Flags().Uint32Var(&healthTimeout, "health-timeout", 30, "Health check timeout (seconds)")
	cmd.Flags().Uint32Var(&healthRetries, "health-retries", 3, "Consecutive failures before marking unhealthy")
	cmd.Flags().Uint32Var(&healthStartPeriod, "health-start-period", 0, "Grace period before health checks start (seconds)")
	cmd.Flags().StringVar(&restartPolicy, "restart", "no", "Restart policy: no, on-failure, always, unless-stopped")
	cmd.Flags().StringVar(&platform, "platform", "", "Target platform for pull (e.g. linux/amd64, linux/arm64)")
	cmd.Flags().StringSliceVar(&secretNames, "secret", nil, "Mount a secret at /run/secrets/<name> (format: name or name=/custom/path)")
	cmd.Flags().StringSliceVar(&configNames, "config", nil, "Mount a config at /<name> (format: name or name=/custom/path)")
	cmd.Flags().BoolVarP(&attach, "attach", "a", false, "Attach to workload: streams stdout/stderr (vm) or polls for exit code (container)")
	return cmd
}

func parseRestartPolicy(s string) (runtimepb.RestartPolicy, error) {
	switch strings.ToLower(s) {
	case "no", "never":
		return runtimepb.RestartPolicy(1), nil // RESTART_NO
	case "on-failure":
		return runtimepb.RestartPolicy(2), nil // RESTART_ON_FAILURE
	case "always":
		return runtimepb.RestartPolicy(3), nil // RESTART_ALWAYS
	case "unless-stopped":
		return runtimepb.RestartPolicy(4), nil // RESTART_UNLESS_STOPPED
	default:
		return 0, fmt.Errorf("invalid restart policy %q (valid: no, on-failure, always, unless-stopped)", s)
	}
}

func buildNetworkRules(outbound, inbound, publish []string) ([]*runtimepb.NetworkRule, error) {
	var rules []*runtimepb.NetworkRule

	for _, out := range outbound {
		// Format: proto:host:port  (e.g. "tcp:api.example.com:443")
		parts := strings.Split(out, ":")
		if len(parts) != 3 {
			return nil, fmt.Errorf("invalid --allow-outbound %q (want proto:host:port)", out)
		}
		port, err := strconv.ParseUint(parts[2], 10, 16)
		if err != nil {
			return nil, fmt.Errorf("invalid port in %q: %w", out, err)
		}
		rules = append(rules, &runtimepb.NetworkRule{
			Direction: "outbound",
			Protocol:  parts[0],
			Port:      uint32(port),
			ToHost:    parts[1],
		})
	}

	for _, portStr := range inbound {
		port, err := strconv.ParseUint(portStr, 10, 16)
		if err != nil {
			return nil, fmt.Errorf("invalid inbound port %q: %w", portStr, err)
		}
		rules = append(rules, &runtimepb.NetworkRule{
			Direction: "inbound",
			Protocol:  "tcp",
			Port:      uint32(port),
		})
	}

	for _, pub := range publish {
		// Format: "host_port:container_port" or just "port"
		parts := strings.Split(pub, ":")
		switch len(parts) {
		case 1:
			port, err := strconv.ParseUint(parts[0], 10, 16)
			if err != nil {
				return nil, fmt.Errorf("invalid --publish port %q: %w", pub, err)
			}
			rules = append(rules, &runtimepb.NetworkRule{
				Direction: "inbound",
				Protocol:  "tcp",
				Port:      uint32(port),
			})
		case 2:
			hostPort, err := strconv.ParseUint(parts[0], 10, 16)
			if err != nil {
				return nil, fmt.Errorf("invalid --publish host port %q: %w", parts[0], err)
			}
			containerPort, err := strconv.ParseUint(parts[1], 10, 16)
			if err != nil {
				return nil, fmt.Errorf("invalid --publish container port %q: %w", parts[1], err)
			}
			rules = append(rules, &runtimepb.NetworkRule{
				Direction: "inbound",
				Protocol:  "tcp",
				Port:      uint32(containerPort),
				HostPort:  uint32(hostPort),
			})
		default:
			return nil, fmt.Errorf("invalid --publish %q (want host_port:container_port or port)", pub)
		}
	}

	return rules, nil
}

func NewStopCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "stop [ID]",
		Short: "Stop a workload",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.StopWorkload(ctx, args[0])
			if err != nil {
				return fmt.Errorf("stop %s: %w", args[0], err)
			}
			if !resp.Success {
				fmt.Fprintf(os.Stderr, "workload %s was not running\n", args[0])
				return nil
			}
			fmt.Printf("✓ stopped %s\n", args[0])
			return nil
		},
	}
	return cmd
}

func NewListCommand(opts *RootOptions) *cobra.Command {
	var asJSON bool

	cmd := &cobra.Command{
		Use:   "list",
		Short: "List running workloads",
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.ListWorkloads(ctx)
			if err != nil {
				return fmt.Errorf("list workloads: %w", err)
			}

			if asJSON {
				enc := json.NewEncoder(os.Stdout)
				enc.SetIndent("", "  ")
				if err := enc.Encode(resp.Workloads); err != nil {
					return fmt.Errorf("encode JSON: %w", err)
				}
				return nil
			}

			if len(resp.Workloads) == 0 {
				fmt.Println("No workloads running.")
				return nil
			}

			fmt.Printf("%-20s %-12s %-12s %-16s %s\n", "ID", "STATE", "BACKEND", "IP", "EXIT")
			for _, w := range resp.Workloads {
				exitStr := "-"
				if w.ExitCode > 0 || w.State == "exited" {
					exitStr = fmt.Sprintf("%d", w.ExitCode)
				}
				fmt.Printf("%-20s %-12s %-12s %-16s %s\n",
					w.Id, w.State, w.Backend, w.InternalIp, exitStr)
			}
			return nil
		},
	}
	cmd.Flags().BoolVar(&asJSON, "json", false, "Output as JSON")
	return cmd
}

func NewGetCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "get [ID]",
		Short: "Get details about a specific workload",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			w, err := client.GetWorkload(ctx, args[0])
			if err != nil {
				return fmt.Errorf("get %s: %w", args[0], err)
			}

			fmt.Printf("ID:        %s\n", w.Id)
			fmt.Printf("State:     %s\n", w.State)
			fmt.Printf("Backend:   %s\n", w.Backend)
			if w.InternalIp != "" {
				fmt.Printf("IP:        %s\n", w.InternalIp)
			}
			if w.StartTime > 0 {
				fmt.Printf("Started:   %s\n", time.Unix(w.StartTime, 0).Format(time.RFC3339))
			}
			fmt.Printf("Isolated:  %v\n", w.NetworkIsolated)
			return nil
		},
	}
	return cmd
}

func NewLogsCommand(opts *RootOptions) *cobra.Command {
	var follow bool
	var tail int64

	cmd := &cobra.Command{
		Use:   "logs [ID]",
		Short: "Stream logs from a workload",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			stream, err := client.StreamLogs(ctx, args[0], follow, tail)
			if err != nil {
				return fmt.Errorf("stream logs: %w", err)
			}

			for {
				chunk, err := stream.Recv()
				if err != nil {
					if err.Error() == "EOF" {
						return nil
					}
					return err
				}
				if chunk.Stderr {
					os.Stderr.Write(chunk.Data)
				} else {
					os.Stdout.Write(chunk.Data)
				}
			}
		},
	}
	cmd.Flags().BoolVarP(&follow, "follow", "f", false, "Follow log output")
	cmd.Flags().Int64Var(&tail, "tail", 100, "Number of lines to tail from the end")
	return cmd
}

func NewExecCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "exec [ID] -- [COMMAND...]",
		Short: "Execute a command in a running workload",
		Args:  cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			id := args[0]
			command := args[1:]

			resp, err := client.ExecInWorkload(ctx, id, command)
			if err != nil {
				return fmt.Errorf("exec in %s: %w", id, err)
			}

			os.Stdout.Write(resp.Stdout)
			os.Stderr.Write(resp.Stderr)
			if resp.ExitCode != 0 {
				os.Exit(int(resp.ExitCode))
			}
			return nil
		},
	}
	return cmd
}

// spawnRuntime starts nimbus-runtime as a child process in daemon mode.
func spawnRuntime(opts *RootOptions) error {
	runtimeBinary, err := findRuntimeBinary()
	if err != nil {
		return err
	}

	storeRoot := os.Getenv("NIMBUS_STORE")
	if storeRoot == "" {
		home, _ := os.UserHomeDir()
		storeRoot = filepath.Join(home, ".local/share/nimbus")
	}
	if err := os.MkdirAll(storeRoot, 0o755); err != nil {
		return err
	}

	cmd := exec.Command(runtimeBinary,
		"daemon",
		"--socket", opts.SocketPath,
		"--store-root", storeRoot,
	)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	doneCh := make(chan struct{})
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		select {
		case <-sigCh:
			if cmd.Process != nil {
				cmd.Process.Signal(syscall.SIGTERM)
			}
		case <-doneCh:
		}
	}()

	if err := cmd.Start(); err != nil {
		signal.Stop(sigCh)
		close(doneCh)
		return fmt.Errorf("start runtime: %w", err)
	}

	// Wait for socket to appear (up to 5 seconds)
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(opts.SocketPath); err == nil {
			signal.Stop(sigCh)
			close(doneCh)
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	signal.Stop(sigCh)
	close(doneCh)
	return fmt.Errorf("runtime socket %s did not appear within 5s", opts.SocketPath)
}

func findRuntimeBinary() (string, error) {
	if path, err := exec.LookPath("nimbus-runtime"); err == nil {
		return path, nil
	}
	return "", fmt.Errorf("nimbus-runtime not found in PATH: install it or check $PATH")
}

// attachToWorkload opens a bidirectional AttachWorkload stream to a running
// workload and proxies the terminal's stdio. It blocks until the workload
// exits or the context is cancelled. If command/env/workdir are non-nil/non-empty
// they override the workload's entrypoint; pass nil/"" to keep defaults.
//
// The caller is responsible for signal handling (context cancellation on ^C).
func attachToWorkload(ctx context.Context, client *GRPCClient, workloadID string, command []string, env map[string]string, workingDir string) error {
	stream, err := client.AttachWorkload(ctx)
	if err != nil {
		return fmt.Errorf("attach: %w", err)
	}

	if err := stream.Send(&runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_Open{
			Open: &runtimepb.AttachOpen{
				WorkloadId: workloadID,
				Command:    command,
				Env:        env,
				WorkingDir: workingDir,
			},
		},
	}); err != nil {
		return fmt.Errorf("send open: %w", err)
	}

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

	exitCode := 0
	for {
		msg, err := stream.Recv()
		if err == io.EOF {
			break
		}
		if err != nil {
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

	<-stdinDone

	if exitCode != 0 {
		os.Exit(exitCode)
	}
	return nil
}

// streamAndWait polls a container-backed workload until it exits and
// propagates its exit code. Container stdout/stderr is not yet captured
// by the runtime (runc executor uses Stdio::null()), so this shows
// status transitions only. AttachWorkload bidi streaming is used for
// the vm backend instead — see attachToWorkload.
func streamAndWait(ctx context.Context, client *GRPCClient, workloadID string) error {
	attachCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		fmt.Fprintln(os.Stderr, "\n[nimbus] detached")
		cancel()
	}()

	pollInterval := 500 * time.Millisecond
	for {
		select {
		case <-attachCtx.Done():
			return nil
		case <-time.After(pollInterval):
		}

		status, err := client.GetWorkload(attachCtx, workloadID)
		if err != nil {
			if attachCtx.Err() != nil {
				return nil
			}
			return fmt.Errorf("get status: %w", err)
		}
		if status.State == "exited" || status.State == "stopped" {
			if status.ExitCode != 0 {
				os.Exit(int(status.ExitCode))
			}
			return nil
		}
	}
}
