// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//go:build windows

package cmd

import (
	"context"
	"fmt"
	"net"
	"os/exec"
	"time"

	runtimepb "pullrun/protoapi/pullrun/runtime"
)

const DefaultSocketPath = "localhost:9501"

func spawnRuntime(opts *RootOptions) error {
	if err := ensureWSLRunning(); err != nil {
		return err
	}

	if !servicesRunning() {
		if err := startServices(); err != nil {
			return err
		}
	}

	return waitForDaemon()
}

func ensureWSLRunning() error {
	if err := exec.Command("wsl", "-d", "Ubuntu", "--exec", "echo", "ready").Run(); err != nil {
		if err := exec.Command("wsl", "-d", "Ubuntu").Start(); err != nil {
			return fmt.Errorf("start WSL Ubuntu: %w", err)
		}
		time.Sleep(3 * time.Second)
	}
	return nil
}

func servicesRunning() bool {
	r1 := exec.Command("wsl", "-d", "Ubuntu", "-u", "root",
		"systemctl", "is-active", "--quiet", "pullrun-runtime").Run()
	r2 := exec.Command("wsl", "-d", "Ubuntu", "-u", "root",
		"systemctl", "is-active", "--quiet", "pullrun-tcp-proxy").Run()
	return r1 == nil && r2 == nil
}

func startServices() error {
	cmd := exec.Command("wsl", "-d", "Ubuntu", "-u", "root",
		"systemctl", "start", "pullrun-runtime", "pullrun-tcp-proxy")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("start WSL services: %w\n%s", err, string(out))
	}
	// Daemon needs time to initialize after start.
	return nil
}

func waitForDaemon() error {
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		if err := checkTCPPort("localhost:9501"); err == nil {
			// Port is open — verify the daemon responds to gRPC.
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
			client, err := NewGRPCClientTCP("localhost:9501")
			if err == nil {
				_, err = client.RuntimeInfo(ctx, &runtimepb.InfoRequest{})
				client.Close()
				cancel()
				if err == nil {
					return nil
				}
				continue
			}
			cancel()
		}
		time.Sleep(500 * time.Millisecond)
	}
	return fmt.Errorf("WSL daemon on localhost:9501 did not become reachable within 15s")
}

func checkTCPPort(addr string) error {
	conn, err := net.DialTimeout("tcp", addr, 2*time.Second)
	if err != nil {
		return err
	}
	conn.Close()
	return nil
}
