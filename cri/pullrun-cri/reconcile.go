// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"log"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

// ============================================================
// Startup reconciliation — the shim's local store can survive
// restarts, but the workloads behind it may have died (or the
// runtime itself restarted and cleaned up). At startup we query
// the runtime's live workload list and mark missing sandboxes /
// containers as exited so kubelet sees truthful state instead of
// phantom "running" pods.
// ============================================================

func (c *criServer) reconcileStartup(ctx context.Context) {
	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()

	resp, err := c.runtimeClient.ListWorkloads(ctx, &pullrunruntime.ListWorkloadsRequest{})
	if err != nil {
		log.Printf("reconcile: cannot list workloads (%v); keeping stored state as-is", err)
		return
	}

	alive := make(map[string]bool, len(resp.Workloads))
	for _, wl := range resp.Workloads {
		switch wl.State {
		case "running", "created", "scheduled":
			alive[wl.Id] = true
		}
	}

	reconciledSandboxes, reconciledContainers := 0, 0

	for _, rec := range c.sandboxStore.allSandboxes() {
		if !alive[rec.pullrunID] {
			rec.state = runtimeapi.PodSandboxState_SANDBOX_NOTREADY
			rec.internalIP = ""
			reconciledSandboxes++
		}
	}

	for _, rec := range c.sandboxStore.allContainers() {
		if !alive[rec.pullrunID] {
			rec.state = runtimeapi.ContainerState_CONTAINER_EXITED
			if rec.exitCode == 0 {
				rec.exitCode = 137 // killed while the shim was down
			}
			reconciledContainers++
		}
	}

	log.Printf("reconcile: %d sandboxes, %d containers marked exited (runtime has %d live workloads)",
		reconciledSandboxes, reconciledContainers, len(alive))
}
