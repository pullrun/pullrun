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

// podBridgeName derives a stable per-pod bridge name from the pod id.
// Linux interface names are capped at 15 characters (IFNAMSIZ), so we
// keep a short prefix plus a truncated hash of the id.
func podBridgeName(podID string) string {
	const prefix = "pr-"
	if len(podID) > 12 {
		podID = podID[len(podID)-12:]
	}
	return prefix + podID
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
	hostNetwork := req.Config.GetLinux() != nil && req.Config.GetLinux().GetSecurityContext().GetNamespaceOptions().GetNetwork() == runtimeapi.NamespaceMode_NODE
	if hostNetwork {
		netMode = "host"
	}
	cpu, mem := parseResourceAnnotations(req.Config.Annotations)

	log.Printf("RunPodSandbox id=%s ns=%s name=%s image=%s backend=%s net=%s hostNetwork=%v cpu=%d mem=%d",
		req.Config.Metadata.Uid,
		req.Config.Metadata.Namespace,
		req.Config.Metadata.Name,
		image, backend, netMode, hostNetwork, cpu, mem)

	// 2. Pull the image into the DAG store (fast path: HasImage first so
	// pods already in the store skip the network entirely).
	rootDigest, err := c.ensureImage(ctx, image, "")
	if err != nil {
		return nil, err
	}

	// Per-pod bridge for pod-level network isolation: every pod gets its
	// own /24 subnet, so pod traffic is isolated even inside the host.
	bridgeName := ""
	if netMode == "bridge" {
		bridgeName = podBridgeName(req.Config.Metadata.Uid)
	}

	// 3. Run the pause workload. It anchors the pod's network namespace;
	// containers created later join it via network_mode "container:<id>".
	runCtx, cancel := context.WithTimeout(ctx, 60*time.Second)
	runResp, err := c.runtimeClient.RunWorkload(runCtx, &pullrunruntime.RunRequest{
		Id:            req.Config.Metadata.Uid,
		RootDigest:    rootDigest,
		Backend:       backend,
		NetworkMode:   netMode,
		CpuMillicores: cpu,
		MemoryBytes:   mem,
		BridgeName:    bridgeName,
	})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("run workload: %w", err)
	}

	// 4. Record the sandbox in our local index.
	rec := &sandboxRecord{
		id:           req.Config.Metadata.Uid,
		pullrunID:    runResp.Id,
		namespace:    req.Config.Metadata.Namespace,
		name:         req.Config.Metadata.Name,
		createdAt:    time.Now(),
		state:        runtimeapi.PodSandboxState_SANDBOX_READY,
		internalIP:   runResp.InternalIp,
		runtimeClass: req.RuntimeHandler,
		bridgeName:   bridgeName,
		hostNetwork:  hostNetwork,
	}
	c.sandboxStore.putSandbox(rec)

	// Return the kubelet-supplied UID as PodSandboxId so every subsequent
	// CRI call (StopPodSandbox, CreateContainer, etc.) can find the record.
	// runResp.Id is the internal pullrun workload ID, stored in rec.pullrunID.
	return &runtimeapi.RunPodSandboxResponse{
		PodSandboxId: req.Config.Metadata.Uid,
	}, nil
}

// ensureImage returns the root digest for an image, pulling it if needed.
func (c *criServer) ensureImage(ctx context.Context, imageRef, registry string) (string, error) {
	hasCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	resp, err := c.runtimeClient.HasImage(hasCtx, &pullrunruntime.HasImageRequest{ImageRef: imageRef})
	cancel()
	if err == nil && resp.Exists && resp.RootDigest != "" {
		return resp.RootDigest, nil
	}

	log.Printf("pulling image %s", imageRef)
	pullCtx, cancel := context.WithTimeout(ctx, 10*60*time.Second)
	pullResp, err := c.runtimeClient.PullImage(pullCtx, &pullrunruntime.PullImageRequest{
		ImageRef: imageRef,
		Registry: registry,
	})
	cancel()
	if err != nil {
		return "", fmt.Errorf("pull %s: %w", imageRef, err)
	}
	return pullResp.RootDigest, nil
}

func (c *criServer) StopPodSandbox(ctx context.Context, req *runtimeapi.StopPodSandboxRequest) (*runtimeapi.StopPodSandboxResponse, error) {
	log.Printf("StopPodSandbox id=%s", req.PodSandboxId)

	// Force-stop any container workloads first so their runc state is
	// cleaned up before the sandbox netns goes away.
	for _, rec := range c.sandboxStore.allContainersForSandbox(req.PodSandboxId) {
		stopCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
		_, _ = c.runtimeClient.StopWorkload(stopCtx, &pullrunruntime.StopRequest{Id: rec.pullrunID})
		cancel()
		rec.state = runtimeapi.ContainerState_CONTAINER_EXITED
		if rec.exitCode == 0 {
			rec.exitCode = 137 // SIGKILL, matching CRI conventions
		}
	}

	stopCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
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
	stopCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
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

	internalIP := ""
	if hasLocal {
		internalIP = rec.internalIP
	}

	getCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	wl, err := c.runtimeClient.GetWorkload(getCtx, &pullrunruntime.GetWorkloadRequest{
		Id: req.PodSandboxId,
	})
	cancel()

	liveState := runtimeapi.PodSandboxState_SANDBOX_NOTREADY
	if err != nil {
		// Workload not found in runtime — treat as notready.
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

	status := &runtimeapi.PodSandboxStatus{
		Id:        req.PodSandboxId,
		State:     liveState,
		CreatedAt: createdAt,
		Network:   &runtimeapi.PodSandboxNetworkStatus{Ip: internalIP},
	}
	if hasLocal {
		status.Metadata = &runtimeapi.PodSandboxMetadata{Name: rec.name, Namespace: rec.namespace, Uid: rec.id}
		status.RuntimeHandler = rec.runtimeClass
	}

	return &runtimeapi.PodSandboxStatusResponse{
		Status: status,
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
	// Real probe: the runtime is ready only if it answers a lightweight RPC.
	probeCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	_, err := c.runtimeClient.ListWorkloads(probeCtx, &pullrunruntime.ListWorkloadsRequest{})
	cancel()

	ready := err == nil
	status := &runtimeapi.RuntimeStatus{
		Conditions: []*runtimeapi.RuntimeCondition{
			{Type: "RuntimeReady", Status: ready},
			{Type: "NetworkReady", Status: ready},
		},
	}
	if !ready {
		log.Printf("Status: runtime not ready: %v", err)
	}
	return &runtimeapi.StatusResponse{Status: status}, nil
}

func (c *criServer) UpdateRuntimeConfig(ctx context.Context, req *runtimeapi.UpdateRuntimeConfigRequest) (*runtimeapi.UpdateRuntimeConfigResponse, error) {
	// v0: no config to apply. The runtime is configured via daemon flags.
	return &runtimeapi.UpdateRuntimeConfigResponse{}, nil
}

func (c *criServer) RuntimeConfig(ctx context.Context, req *runtimeapi.RuntimeConfigRequest) (*runtimeapi.RuntimeConfigResponse, error) {
	return &runtimeapi.RuntimeConfigResponse{}, nil
}
