package cmd

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
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

func NewPullCommand(opts *RootOptions) *cobra.Command {
	var registry string

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
			resp, err := client.PullImage(ctx, imageRef, registry, auth)
			if err != nil {
				return fmt.Errorf("pull %s: %w", imageRef, err)
			}

			fmt.Printf("✓ %s\n", imageRef)
			fmt.Printf("  root digest: %s\n", resp.RootDigest)
			fmt.Printf("  stored:      %d bytes\n", resp.BytesStored)
			if resp.BytesDeduplicated > 0 {
				fmt.Printf("  deduped:     %d bytes\n", resp.BytesDeduplicated)
			}
			return nil
		},
	}
	cmd.Flags().StringVar(&registry, "registry", "docker.io", "Registry to pull from")
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
				pullResp, err := client.PullImage(ctx, rootDigest, pullRegistry, auth)
				if err != nil {
					return fmt.Errorf("pull %s: %w", rootDigest, err)
				}
				rootDigest = pullResp.RootDigest
				fmt.Fprintf(os.Stderr, "pulled %s -> %s\n", args[0], rootDigest)
			}

			// Build network rules
			rules, err := buildNetworkRules(allowOutbound, allowInbound)
			if err != nil {
				return err
			}

			// Parse env vars
			for _, e := range envVars {
				parts := strings.SplitN(e, "=", 2)
				if len(parts) != 2 {
					return fmt.Errorf("invalid env var %q (expected KEY=VALUE)", e)
				}
				envMap[parts[0]] = parts[1]
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
			})
			if err != nil {
				return fmt.Errorf("run workload: %w", err)
			}

			fmt.Printf("Started %s\n", resp.Id)
			fmt.Printf("  backend:    %s\n", resp.BackendUsed)
			fmt.Printf("  pid:        %d\n", resp.Pid)
			if resp.InternalIp != "" {
				fmt.Printf("  internal:   %s\n", resp.InternalIp)
			}
			return nil
		},
	}

	cmd.Flags().StringVar(&backend, "backend", "container", "Backend: container, vm, sandbox")
	cmd.Flags().StringVar(&name, "name", "", "Workload name (auto-generated if empty)")
	cmd.Flags().StringSliceVar(&allowOutbound, "allow-outbound", nil, "Allow outbound: tcp:host:port")
	cmd.Flags().StringSliceVar(&allowInbound, "allow-inbound", nil, "Allow inbound port (e.g. 8080)")
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
	return cmd
}

func buildNetworkRules(outbound, inbound []string) ([]*runtimepb.NetworkRule, error) {
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
				// Simple JSON output (no extra dependency)
				fmt.Println("[")
				for i, w := range resp.Workloads {
					if i > 0 {
						fmt.Print(",\n")
					}
					fmt.Printf(`  {"id":%q,"state":%q,"backend":%q,"ip":%q,"pid":%d}`,
						w.Id, w.State, w.Backend, w.InternalIp, w.ExitCode)
				}
				fmt.Println("\n]")
				return nil
			}

			if len(resp.Workloads) == 0 {
				fmt.Println("No workloads running.")
				return nil
			}

			fmt.Printf("%-20s %-12s %-12s %-16s %s\n", "ID", "STATE", "BACKEND", "IP", "PID")
			for _, w := range resp.Workloads {
				pidStr := "-"
				if w.ExitCode > 0 {
					pidStr = fmt.Sprintf("%d", w.ExitCode)
				}
				fmt.Printf("%-20s %-12s %-12s %-16s %s\n",
					w.Id, w.State, w.Backend, w.InternalIp, pidStr)
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
	runtimeBinary, _ := findRuntimeBinary()

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

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		if cmd.Process != nil {
			cmd.Process.Signal(syscall.SIGTERM)
		}
	}()

	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start runtime: %w", err)
	}

	// Wait for socket to appear (up to 5 seconds)
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(opts.SocketPath); err == nil {
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	return fmt.Errorf("runtime socket %s did not appear within 5s", opts.SocketPath)
}

func findRuntimeBinary() (string, error) {
	if path, err := exec.LookPath("nimbus-runtime"); err == nil {
		return path, nil
	}
	return "nimbus-runtime", nil
}
