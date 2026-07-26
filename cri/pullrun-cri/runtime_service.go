// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"log"
	"strconv"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

// backendForRuntimeHandler maps CRI's RuntimeHandler (the RuntimeClass name) to
// a Pullrun backend identifier.
func backendForRuntimeHandler(handler string) string {
	switch handler {
	case PullrunVMRuntimeClass, "vm", "firecracker":
		return "vm"
	case PullrunContainerRuntimeClass, "", "container", "runc":
		return "container"
	default:
		log.Printf("unknown RuntimeHandler %q, defaulting to container", handler)
		return "container"
	}
}

// runWorkloadImageFromAnnotations pulls a workload image from a pod's
// annotations, falling back to the standard pause image when no override is set.
//
//   - `pullrun.io/image`: full image ref (e.g. "alpine:latest", "ghcr.io/me/app:v1")
//   - `pullrun.io/command`: optional command override (JSON array as a string)
func runWorkloadImageFromAnnotations(annotations map[string]string) string {
	if annotations != nil {
		if img, ok := annotations[AnnotationPullrunImage]; ok && img != "" {
			return img
		}
	}
	return DefaultPauseImage
}

// parseResourceAnnotations extracts optional resource limits from pod annotations.
func parseResourceAnnotations(annotations map[string]string) (cpu uint64, mem uint64) {
	if annotations == nil {
		return 0, 0
	}
	if v, ok := annotations[AnnotationPullrunCPUMillicores]; ok && v != "" {
		if n, err := strconv.ParseUint(v, 10, 64); err == nil {
			cpu = n
		}
	}
	if v, ok := annotations[AnnotationPullrunMemoryBytes]; ok && v != "" {
		if n, err := strconv.ParseUint(v, 10, 64); err == nil {
			mem = n
		}
	}
	return
}

// ============================================================
// Critical path: RunPodSandbox / StopPodSandbox / PodSandboxStatus
// / ListPodSandbox / Version
// ============================================================

func (c *criServer) RunPodSandbox(ctx context.Context, req *runtimeapi.RunPodSandboxRequest) (*runtimeapi.RunPodSandboxResponse, error) {
	if req.Config == nil || req.Config.Metadata == nil {
		return nil, fmt.Errorf("RunPodSandbox: missing sandbox config/metadata")
	}

	// 1. Determine the image to run. K8s will normally not set one, so we use
	// the pause image. Users override via `pullrun.io/image` annotation.
	image := runWorkloadImageFromAnnotations(req.Config.Annotations)
	backend := backendForRuntimeHandler(req.RuntimeHandler)
	netMode := c.networkMode
	if netMode == "" {
		netMode = "isolated"
	}
	cpu, mem := parseResourceAnnotations(req.Config.Annotations)

	log.Printf("RunPodSandbox id=%s ns=%s name=%s image=%s backend=%s net=%s cpu=%d mem=%d",
		req.Config.Metadata.Uid,
		req.Config.Metadata.Namespace,
		req.Config.Metadata.Name,
		image, backend, netMode, cpu, mem)

	// 2. Pull the image into the DAG store.
	pullCtx, cancel := context.WithTimeout(ctx, 10*60*1e9) // 10 min
	// Empty Registry → runtime uses default (registry-1.docker.io).
	pullResp, err := c.runtimeClient.PullImage(pullCtx, &pullrunruntime.PullImageRequest{
		ImageRef: image,
		Registry: "",
	})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("pull %s: %w", image, err)
	}

	// 3. Run it as a Pullrun workload. Use the K8s UID as the Pullrun ID so
	// podSandboxId maps 1:1 to workloadId.
	runCtx, cancel := context.WithTimeout(ctx, 60*1e9) // 60s
	runResp, err := c.runtimeClient.RunWorkload(runCtx, &pullrunruntime.RunRequest{
		Id:           req.Config.Metadata.Uid,
		RootDigest:   pullResp.RootDigest,
		Backend:      backend,
		NetworkMode:  netMode,
		CpuMillicores: cpu,
		MemoryBytes:  mem,
	})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("run workload: %w", err)
	}

	// 4. Record the sandbox in our local index.
	rec := &sandboxRecord{
		id:           req.Config.Metadata.Uid,
		pullrunID:     runResp.Id,
		namespace:    req.Config.Metadata.Namespace,
		name:         req.Config.Metadata.Name,
		createdAt:    time.Now(),
		state:        runtimeapi.PodSandboxState_SANDBOX_READY,
		internalIP:   runResp.InternalIp,
		runtimeClass: req.RuntimeHandler,
	}
	c.sandboxStore.putSandbox(rec)

	// Return the kubelet-supplied UID as PodSandboxId so every subsequent
	// CRI call (StopPodSandbox, CreateContainer, etc.) can find the record.
	// runResp.Id is the internal pullrun workload ID, stored in rec.pullrunID.
	return &runtimeapi.RunPodSandboxResponse{
		PodSandboxId: req.Config.Metadata.Uid,
	}, nil
}

func (c *criServer) StopPodSandbox(ctx context.Context, req *runtimeapi.StopPodSandboxRequest) (*runtimeapi.StopPodSandboxResponse, error) {
	log.Printf("StopPodSandbox id=%s", req.PodSandboxId)

	stopCtx, cancel := context.WithTimeout(ctx, 30*1e9)
	_, err := c.runtimeClient.StopWorkload(stopCtx, &pullrunruntime.StopRequest{
		Id: req.PodSandboxId,
	})
	cancel()

	if err != nil {
		return nil, fmt.Errorf("stop workload: %w", err)
	}

	// Update local state.
	if rec, ok := c.sandboxStore.getSandbox(req.PodSandboxId); ok {
		rec.state = runtimeapi.PodSandboxState_SANDBOX_NOTREADY
	}

	return &runtimeapi.StopPodSandboxResponse{}, nil
}

func (c *criServer) RemovePodSandbox(ctx context.Context, req *runtimeapi.RemovePodSandboxRequest) (*runtimeapi.RemovePodSandboxResponse, error) {
	log.Printf("RemovePodSandbox id=%s", req.PodSandboxId)

	// Best-effort: stop the workload first (ignore errors if already gone).
	stopCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	_, _ = c.runtimeClient.StopWorkload(stopCtx, &pullrunruntime.StopRequest{
		Id: req.PodSandboxId,
	})
	cancel()

	c.sandboxStore.removeSandbox(req.PodSandboxId)
	return &runtimeapi.RemovePodSandboxResponse{}, nil
}

func (c *criServer) PodSandboxStatus(ctx context.Context, req *runtimeapi.PodSandboxStatusRequest) (*runtimeapi.PodSandboxStatusResponse, error) {
	// Prefer the local cache for state, but query pullrun-runtime for live state.
	rec, hasLocal := c.sandboxStore.getSandbox(req.PodSandboxId)

	liveState := runtimeapi.PodSandboxState_SANDBOX_READY
	internalIP := ""
	if hasLocal {
		internalIP = rec.internalIP
	}

	getCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	wl, err := c.runtimeClient.GetWorkload(getCtx, &pullrunruntime.GetWorkloadRequest{
		Id: req.PodSandboxId,
	})
	cancel()

	if err != nil {
		// Workload not found in runtime — treat as notready.
		liveState = runtimeapi.PodSandboxState_SANDBOX_NOTREADY
	} else {
		switch wl.State {
		case "running", "created", "scheduled":
			liveState = runtimeapi.PodSandboxState_SANDBOX_READY
		default:
			liveState = runtimeapi.PodSandboxState_SANDBOX_NOTREADY
		}
		if wl.InternalIp != "" {
			internalIP = wl.InternalIp
		}
	}

	if hasLocal {
		rec.state = liveState
		rec.internalIP = internalIP
	}

	createdAt := int64(0)
	if hasLocal {
		createdAt = rec.createdAt.UnixNano()
	}

	return &runtimeapi.PodSandboxStatusResponse{
		Status: &runtimeapi.PodSandboxStatus{
			Id:        req.PodSandboxId,
			State:     liveState,
			CreatedAt: createdAt,
			Network:   &runtimeapi.PodSandboxNetworkStatus{Ip: internalIP},
		},
	}, nil
}

func (c *criServer) ListPodSandbox(ctx context.Context, req *runtimeapi.ListPodSandboxRequest) (*runtimeapi.ListPodSandboxResponse, error) {
	sandboxes := c.sandboxStore.listSandboxes(req.Filter)
	return &runtimeapi.ListPodSandboxResponse{Items: sandboxes}, nil
}

func (c *criServer) Version(ctx context.Context, req *runtimeapi.VersionRequest) (*runtimeapi.VersionResponse, error) {
	return &runtimeapi.VersionResponse{
		Version:           PullrunCRIVersion,
		RuntimeName:       "pullrun",
		RuntimeVersion:    PullrunCRIVersion,
		RuntimeApiVersion: "v1",
	}, nil
}

func (c *criServer) Status(ctx context.Context, req *runtimeapi.StatusRequest) (*runtimeapi.StatusResponse, error) {
	return &runtimeapi.StatusResponse{
		Status: &runtimeapi.RuntimeStatus{
			Conditions: []*runtimeapi.RuntimeCondition{
				{Type: "RuntimeReady", Status: true},
				{Type: "NetworkReady", Status: true},
			},
		},
	}, nil
}

func (c *criServer) UpdateRuntimeConfig(ctx context.Context, req *runtimeapi.UpdateRuntimeConfigRequest) (*runtimeapi.UpdateRuntimeConfigResponse, error) {
	// v0: no config to apply. The runtime is configured via daemon flags.
	return &runtimeapi.UpdateRuntimeConfigResponse{}, nil
}

func (c *criServer) RuntimeConfig(ctx context.Context, req *runtimeapi.RuntimeConfigRequest) (*runtimeapi.RuntimeConfigResponse, error) {
	return &runtimeapi.RuntimeConfigResponse{}, nil
}
