// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/term"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// processInput scans data for the detach escape sequence (Ctrl-P Ctrl-Q = 0x10 0x11).
// It returns the filtered data (with the escape sequence removed) and whether
// the detach sequence was found. pending10 tracks a leading 0x10 from a previous
// chunk that may be the start of an escape sequence.
//
// The function processes data in bulk using bytes.IndexByte for efficiency,
// only branching per byte when a potential 0x10 prefix is found.
func processInput(data []byte, pending10 *bool) (filtered []byte, detached bool) {
	// Fast path: no 0x10 byte at all — nothing to escape. But if we have
	// a pending 0x10 from a previous chunk, we must still check.
	if !*pending10 && bytes.IndexByte(data, 0x10) < 0 {
		return data, false
	}

	filtered = make([]byte, 0, len(data))
	offset := 0
	if *pending10 {
		// The previous chunk ended with 0x10. Check if this one starts with 0x11.
		if len(data) > 0 && data[0] == 0x11 {
			*pending10 = false
			return nil, true
		}
		filtered = append(filtered, 0x10)
		offset = 0
		*pending10 = false
	}

	for offset < len(data) {
		// Search for the next 0x10 from the current position.
		idx := bytes.IndexByte(data[offset:], 0x10)
		if idx < 0 {
			// No more 0x10 — copy everything remaining.
			filtered = append(filtered, data[offset:]...)
			break
		}

		// Copy everything up to (but not including) the 0x10.
		filtered = append(filtered, data[offset:offset+idx]...)
		pos := offset + idx

		if pos+1 < len(data) && data[pos+1] == 0x11 {
			// 0x10 0x11 found — detach.
			return filtered, true
		}

		if pos == len(data)-1 {
			// 0x10 is the last byte — might be start of escape in next chunk.
			*pending10 = true
			break
		}

		// Standalone 0x10 — keep it and continue.
		filtered = append(filtered, 0x10)
		offset = pos + 1
	}

	return filtered, false
}

// ensureGRPCClient returns a connected gRPC client. If direct mode is enabled
// and the runtime is not running, it spawns pullrun-runtime as a child process.
// If a stale socket is found (daemon died), it replaces it with a fresh daemon.
func ensureGRPCClient(opts *RootOptions) (*GRPCClient, func(), error) {
	if opts.ServerAddr != "" {
		client, err := NewGRPCClientTCP(opts.ServerAddr)
		if err != nil {
			return nil, nil, err
		}
		return client, func() { client.Close() }, nil
	}

	if !opts.DirectMode {
		return dialSocketOrTCP(opts.SocketPath)
	}

	// Direct mode: connect, spawn if needed.
	return ensureClientDirectMode(opts)
}

// isTCPAddr returns true if addr looks like host:port.
func isTCPAddr(addr string) bool {
	return strings.Contains(addr, ":")
}

// dialSocketOrTCP connects via UDS or TCP depending on the address format.
func dialSocketOrTCP(addr string) (*GRPCClient, func(), error) {
	if isTCPAddr(addr) {
		client, err := NewGRPCClientTCP(addr)
		if err != nil {
			return nil, nil, err
		}
		return client, func() { client.Close() }, nil
	}
	client, err := NewGRPCClient(addr)
	if err != nil {
		return nil, nil, err
	}
	return client, func() { client.Close() }, nil
}

// ensureClientDirectMode tries to connect, spawns runtime if needed, retries once.
func ensureClientDirectMode(opts *RootOptions) (*GRPCClient, func(), error) {
	for attempt := 0; attempt < 2; attempt++ {
		// Try connecting first (platform-independent)
		client, closeFn, err := dialSocketOrTCP(opts.SocketPath)
		if err == nil {
			return client, closeFn, nil
		}

		// On first failure, attempt to spawn / start the daemon.
		if attempt == 0 {
			if err := spawnRuntime(opts); err != nil {
				return nil, nil, fmt.Errorf("spawn runtime: %w", err)
			}
		}
	}

	return nil, nil, fmt.Errorf("cannot connect to runtime (tried spawning a new daemon)")
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

// defaultBackend returns "vm" on macOS where runc is
// unavailable, and "container" on all other platforms.
func defaultBackend() string {
	if runtime.GOOS == "darwin" {
		return "vm"
	}
	return "container"
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
		tty             bool
	)

	cmd := &cobra.Command{
		Use:   "run [IMAGE_REF|DIGEST]",
		Short: "Run a workload from a DAG root or image reference",
		Args:  cobra.ExactArgs(1),
		Long: `Run a workload. Accepts either a content-addressed digest (sha256:...)
or an image:tag reference (e.g. alpine:latest) which will be pulled first.

The default backend is "container" on Linux and "vm" on macOS.
Use --backend=vm explicitly on Linux to run inside a Firecracker micro-VM.
The kernel is loaded from ~/.pullrun/kernels/ by default, or from
an OCI image via --kernel-image=<ref>.`,
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
			netMode := networkMode
			if backend == "vm" && !cmd.Flags().Changed("net") && netMode == "isolated" {
				netMode = "slirp"
			}
			resp, err := client.RunWorkload(ctx, &runtimepb.RunRequest{
				Id:            id,
				RootDigest:    rootDigest,
				Backend:       backend,
				Command:       command,
				Env:           envMap,
				CpuMillicores: cpuMillicores,
				MemoryBytes:   memoryBytes,
				NetworkMode:   netMode,
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

				// Use AttachWorkload for both VM and container backends so
				// console output (serial console for VMs, stdout for containers)
				// is streamed back to the client. On macOS with Apple Virt the
				// stream carries vsock-backed PTY frames; on Linux with
				// Firecracker it carries the serial console log; on the
				// container backend it carries the runc exec output.
				//
				// Non-TTY attach (container only) falls back to polling
				// GetWorkload for exit code since there's no output to stream.
				if tty || resp.BackendUsed == "vm" {
					sigCh := make(chan os.Signal, 1)
					signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
					go func() {
						<-sigCh
						fmt.Fprintln(os.Stderr, "\n[pullrun] detached")
						attachCancel()
					}()

					return attachToWorkload(attachCtx, client, resp.Id, nil, nil, "", tty)
				}
				// Poll GetWorkload until the workload exits.
				return streamAndWait(attachCtx, client, resp.Id)
			}

			return nil
		},
	}

	cmd.Flags().StringVar(&backend, "backend", defaultBackend(), "Backend: container, vm, sandbox")
	cmd.Flags().StringVar(&name, "name", "", "Workload name (auto-generated if empty)")
	cmd.Flags().StringSliceVar(&allowOutbound, "allow-outbound", nil, "Allow outbound: tcp:host:port")
	cmd.Flags().StringSliceVar(&allowInbound, "allow-inbound", nil, "Allow inbound port (e.g. 8080)")
	cmd.Flags().StringSliceVarP(&publishPorts, "publish", "p", nil, "Publish host:container port (e.g. 8080:80, or just 8080)")
	cmd.Flags().StringSliceVarP(&envVars, "env", "e", nil, "Environment variables (KEY=VALUE)")
	cmd.Flags().StringSliceVar(&command, "cmd", nil, "Override entrypoint command")
	cmd.Flags().Uint64Var(&cpuMillicores, "cpu", 1000, "CPU millicores (1000 = 1 vCPU)")
	cmd.Flags().Uint64Var(&memoryBytes, "memory", 512*1024*1024, "Memory limit in bytes")
	cmd.Flags().StringVar(&networkMode, "net", "isolated", "Network mode: isolated|host|none|slirp")
	cmd.Flags().StringVar(&kernelImage, "kernel-image", "", "OCI reference for the kernel image (optional on macOS when ~/.pullrun/kernels/ has one, e.g. 'pullrun/kernel-asahi:6.19.14')")
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
	cmd.Flags().BoolVarP(&tty, "tty", "t", false, "Allocate a pseudo-TTY for the workload")
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
	var tty bool
	cmd := &cobra.Command{
		Use:   "exec [ID] -- [COMMAND...]",
		Short: "Execute a command in a running workload",
		Long: `Run a command inside a running workload and display its output.

Without --tty, uses ExecInWorkload (unary RPC) — runs the command,
captures stdout/stderr, and returns the exit code. Works for all
backends (container, Firecracker VM, Apple Virt VM).

With --tty or -t, opens a bidirectional AttachWorkload stream with
a pseudo-terminal — interactive shell with detach via Ctrl-P Ctrl-Q.
Works for container and Apple Virt VM backends.

This is the equivalent of 'docker exec'.`,
		Args: cobra.MinimumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			workloadID := args[0]
			command := args[1:]

			// Check for -t/--tty in command args to support
			// `exec <id> -t -- <cmd>` (docker-exec-style).
			if !tty {
				for i, a := range command {
					if a == "-t" || a == "--tty" {
						tty = true
						command = append(command[:i], command[i+1:]...)
						break
					}
				}
			}

			if tty {
				// Interactive mode: bidi AttachWorkload stream with PTY.
				// We do not check the workload state here — the daemon's
				// run_runc_attach_session handles both running and exited
				// workloads (for exited, it starts a sleep container first
				// and execs into it).
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()

				sigCh := make(chan os.Signal, 1)
				signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
				go func() {
					<-sigCh
					fmt.Fprintln(os.Stderr, "\n[pullrun] detached")
					cancel()
				}()

				return attachToWorkload(ctx, client, workloadID, command, nil, "", tty)
			}

			// Non-interactive mode: unary ExecInWorkload RPC.
			ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
			defer cancel()

			resp, err := client.ExecInWorkload(ctx, workloadID, command)
			if err != nil {
				return fmt.Errorf("exec: %w", err)
			}

			if len(resp.Stdout) > 0 {
				os.Stdout.Write(resp.Stdout)
			}
			if len(resp.Stderr) > 0 {
				os.Stderr.Write(resp.Stderr)
			}

			if resp.ExitCode != 0 {
				os.Exit(int(resp.ExitCode))
			}
			return nil
		},
	}
	cmd.Flags().BoolVarP(&tty, "tty", "t", false, "Allocate a pseudo-TTY for interactive shell access")
	return cmd
}

// attachToWorkload opens a bidirectional AttachWorkload stream to a running
// workload and proxies the terminal's stdio. It blocks until the workload
// exits or the context is cancelled. If command/env/workdir are non-nil/non-empty
// they override the workload's entrypoint; pass nil/"" to keep defaults.
//
// When tty is true, the host terminal is put into raw mode and the guest
// allocates a PTY for the workload.
//
// Detach key: Ctrl-P Ctrl-Q (0x10 0x11) — closes the gRPC stream cleanly
// without killing the workload. Same as ^C but friendlier (no OS signal).
// The caller is responsible for signal handling (context cancellation on ^C).
func attachToWorkload(ctx context.Context, client *GRPCClient, workloadID string, command []string, env map[string]string, workingDir string, tty bool) error {
	// Wrap the context so we can cancel from inside (escape sequence).
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	stream, err := client.AttachWorkload(ctx)
	if err != nil {
		return fmt.Errorf("attach: %w", err)
	}

	if err := stream.Send(&runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_Open{
			Open: &runtimepb.AttachOpen{
				WorkloadId:  workloadID,
				Command:     command,
				Env:         env,
				WorkingDir:  workingDir,
				Tty:         tty,
				InitialRows: func() uint32 {
					if !tty {
						return 0
					}
					if _, h, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
						return uint32(h)
					}
					return 24
				}(),
				InitialCols: func() uint32 {
					if !tty {
						return 0
					}
					if w, _, err := term.GetSize(int(os.Stdin.Fd())); err == nil {
						return uint32(w)
					}
					return 80
				}(),
			},
		},
	}); err != nil {
		return fmt.Errorf("send open: %w", err)
	}

	if tty {
		restore, err := setupRawTerminal()
		if err == nil && restore != nil {
			defer restore()
		}

		go watchWindowSize(stream)
	}

	stdinDone := make(chan struct{})
	go func() {
		defer close(stdinDone)
		buf := make([]byte, 65536)
		var pending10 bool
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				data := buf[:n]
				if tty {
					// Fast path: scan for escape sequence (Ctrl-P Ctrl-Q = 0x10 0x11)
					// in the input buffer without byte-by-byte processing.
					var detached bool
					data, detached = processInput(data, &pending10)
					if detached {
						fmt.Fprintln(os.Stderr, "[pullrun] detached (escape)")
						_ = stream.CloseSend()
						cancel()
						return
					}
				}
				if len(data) > 0 {
					if sendErr := stream.Send(&runtimepb.AttachMessage{
						Body: &runtimepb.AttachMessage_Stdin{
							Stdin: &runtimepb.AttachStdin{Data: data},
						},
					}); sendErr != nil {
						return
					}
				}
			}
			if err == io.EOF {
				if pending10 {
					_ = stream.Send(&runtimepb.AttachMessage{
						Body: &runtimepb.AttachMessage_Stdin{
							Stdin: &runtimepb.AttachStdin{Data: []byte{0x10}},
						},
					})
				}
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

	type recvResult struct {
		msg *runtimepb.AttachMessage
		err error
	}
	recvCh := make(chan recvResult, 64)
	recvCtx, recvCancel := context.WithCancel(ctx)
	defer recvCancel()
	go func() {
		defer recvCancel()
		for {
			msg, err := stream.Recv()
			select {
			case recvCh <- recvResult{msg, err}:
			case <-recvCtx.Done():
				return
			}
			if err != nil {
				return
			}
		}
	}()

	exitCode := 0
loop:
	for {
		select {
		case result := <-recvCh:
			if result.err == io.EOF {
				break loop
			}
			if result.err != nil {
				if ctx.Err() != nil {
					break loop
				}
				return fmt.Errorf("recv: %w", result.err)
			}
			switch body := result.msg.Body.(type) {
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
					fmt.Fprintf(os.Stderr, "[pullrun] runtime error: %s\n", body.Error.Message)
				}
			}
		case <-ctx.Done():
			break loop
		}
	}

	// Close stdin to wake up the stdin goroutine (it may be
	// blocked on os.Stdin.Read() after the workload exits).
	os.Stdin.Close()
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
		fmt.Fprintln(os.Stderr, "\n[pullrun] detached")
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
