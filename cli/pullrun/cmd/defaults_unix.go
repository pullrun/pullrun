//go:build unix || linux || darwin

package cmd

import (
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"
)

const DefaultSocketPath = "/tmp/pullrun.sock"

func spawnRuntime(opts *RootOptions) error {
	runtimeBinary, err := findRuntimeBinary()
	if err != nil {
		return err
	}

	storeRoot := os.Getenv("PULLRUN_STORE")
	if storeRoot == "" {
		home, _ := os.UserHomeDir()
		storeRoot = filepath.Join(home, ".local/share/pullrun")
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
	if path, err := exec.LookPath("pullrun-runtime"); err == nil {
		return path, nil
	}
	if exe, err := os.Executable(); err == nil {
		candidate := filepath.Join(filepath.Dir(exe), "pullrun-runtime")
		if _, err := os.Stat(candidate); err == nil {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("pullrun-runtime not found: install it via 'make build' or place it alongside this binary")
}
