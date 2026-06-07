package main

import (
	"context"
	"fmt"
	"log"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	nimbusruntime "nimbus/protoapi/nimbus/runtime"
)

// ============================================================
// Container service — v0: 1:1 with sandbox (no pod-in-pod sharing).
// A CRI "container" maps to a Nimbus workload that runs in the same network
// namespace as its parent sandbox. A future revision will add true pod-in-VM
// semantics with multiple processes in a single Firecracker VM.
// ============================================================

func (c *criServer) CreateContainer(ctx context.Context, req *runtimeapi.CreateContainerRequest) (*runtimeapi.CreateContainerResponse, error) {
	if req.PodSandboxId == "" {
		return nil, fmt.Errorf("CreateContainer: missing PodSandboxId")
	}
	if req.Config == nil || req.Config.Metadata == nil {
		return nil, fmt.Errorf("CreateContainer: missing container config/metadata")
	}
	if _, ok := c.sandboxStore.getSandbox(req.PodSandboxId); !ok {
		return nil, fmt.Errorf("CreateContainer: sandbox %q not found", req.PodSandboxId)
	}

	// v0: we already have a workload running for the sandbox; the container
	// is essentially a label on it. The containerID is the workloadID of the
	// sandbox (so Start/Stop/Exec operate on the same workload).
	cid := req.Config.Metadata.Name + "-" + req.PodSandboxId[:8]
	rec := &containerRecord{
		id:        cid,
		sandboxID: req.PodSandboxId,
		nimbusID:  req.PodSandboxId, // share workload with sandbox
		name:      req.Config.Metadata.Name,
		image:     req.Config.Image.Image,
		createdAt: time.Now(),
	}
	c.sandboxStore.putContainer(rec)

	log.Printf("CreateContainer id=%s sandbox=%s image=%s", cid, req.PodSandboxId, req.Config.Image.Image)
	return &runtimeapi.CreateContainerResponse{ContainerId: cid}, nil
}

func (c *criServer) StartContainer(ctx context.Context, req *runtimeapi.StartContainerRequest) (*runtimeapi.StartContainerResponse, error) {
	// v0: container shares workload with sandbox. If the sandbox is running,
	// the container is "started" too.
	if _, ok := c.sandboxStore.getContainer(req.ContainerId); !ok {
		return nil, fmt.Errorf("StartContainer: container %q not found", req.ContainerId)
	}
	log.Printf("StartContainer id=%s (no-op in v0)", req.ContainerId)
	return &runtimeapi.StartContainerResponse{}, nil
}

func (c *criServer) StopContainer(ctx context.Context, req *runtimeapi.StopContainerRequest) (*runtimeapi.StopContainerResponse, error) {
	// v0: stopping the container would stop the entire sandbox. K8s typically
	// stops containers before sandboxes, so we stop the underlying workload
	// here (the sandbox stop will be a no-op).
	rec, ok := c.sandboxStore.getContainer(req.ContainerId)
	if !ok {
		return nil, fmt.Errorf("StopContainer: container %q not found", req.ContainerId)
	}

	stopCtx, cancel := context.WithTimeout(ctx, 30*1e9)
	_, err := c.runtimeClient.StopWorkload(stopCtx, &nimbusruntime.StopRequest{Id: rec.nimbusID})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("stop workload: %w", err)
	}
	log.Printf("StopContainer id=%s (stopped workload %s)", req.ContainerId, rec.nimbusID)
	return &runtimeapi.StopContainerResponse{}, nil
}

func (c *criServer) RemoveContainer(ctx context.Context, req *runtimeapi.RemoveContainerRequest) (*runtimeapi.RemoveContainerResponse, error) {
	// v0: removing a container doesn't tear down the sandbox.
	rec, ok := c.sandboxStore.getContainer(req.ContainerId)
	if !ok {
		return nil, fmt.Errorf("RemoveContainer: container %q not found", req.ContainerId)
	}
	c.sandboxStore.mu.Lock()
	delete(c.sandboxStore.containers, rec.id)
	c.sandboxStore.mu.Unlock()
	log.Printf("RemoveContainer id=%s", req.ContainerId)
	return &runtimeapi.RemoveContainerResponse{}, nil
}

func (c *criServer) ContainerStatus(ctx context.Context, req *runtimeapi.ContainerStatusRequest) (*runtimeapi.ContainerStatusResponse, error) {
	rec, ok := c.sandboxStore.getContainer(req.ContainerId)
	if !ok {
		return nil, fmt.Errorf("ContainerStatus: container %q not found", req.ContainerId)
	}

	// Query the live state of the underlying workload.
	getCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	wl, err := c.runtimeClient.GetWorkload(getCtx, &nimbusruntime.GetWorkloadRequest{Id: rec.nimbusID})
	cancel()

	state := runtimeapi.ContainerState_CONTAINER_CREATED
	if err == nil {
		switch wl.State {
		case "running":
			state = runtimeapi.ContainerState_CONTAINER_RUNNING
		case "exited", "stopped":
			state = runtimeapi.ContainerState_CONTAINER_EXITED
		}
	}

	return &runtimeapi.ContainerStatusResponse{
		Status: &runtimeapi.ContainerStatus{
			Id:        rec.id,
			Metadata:  &runtimeapi.ContainerMetadata{Name: rec.name},
			State:     state,
			CreatedAt: rec.createdAt.UnixNano(),
			Image:     &runtimeapi.ImageSpec{Image: rec.image},
			ImageRef:  rec.image,
			// PodSandboxId is in Container, not ContainerStatus, in CRI v1.
		},
	}, nil
}

func (c *criServer) ListContainers(ctx context.Context, req *runtimeapi.ListContainersRequest) (*runtimeapi.ListContainersResponse, error) {
	items := c.sandboxStore.listContainers(req.Filter)
	return &runtimeapi.ListContainersResponse{Containers: items}, nil
}

func (c *criServer) UpdateContainerResources(ctx context.Context, req *runtimeapi.UpdateContainerResourcesRequest) (*runtimeapi.UpdateContainerResourcesResponse, error) {
	// v0: resources are pinned at workload creation. A real implementation
	// would push cgroup/vm updates through the runtime.
	log.Printf("UpdateContainerResources id=%s (no-op in v0)", req.ContainerId)
	return &runtimeapi.UpdateContainerResourcesResponse{}, nil
}

func (c *criServer) ReopenContainerLog(ctx context.Context, req *runtimeapi.ReopenContainerLogRequest) (*runtimeapi.ReopenContainerLogResponse, error) {
	// v0: logs are streamed on demand; reopen is a no-op.
	return &runtimeapi.ReopenContainerLogResponse{}, nil
}

// ============================================================
// Not implemented in v0 (will be added with stream gRPC endpoints)
// ============================================================

func (c *criServer) ExecSync(ctx context.Context, req *runtimeapi.ExecSyncRequest) (*runtimeapi.ExecSyncResponse, error) {
	rec, ok := c.sandboxStore.getContainer(req.ContainerId)
	if !ok {
		return nil, fmt.Errorf("ExecSync: container %q not found", req.ContainerId)
	}

	execCtx, cancel := context.WithTimeout(ctx, 60*1e9)
	resp, err := c.runtimeClient.ExecInWorkload(execCtx, &nimbusruntime.ExecRequest{
		Id:      rec.nimbusID,
		Command: req.Cmd,
	})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("exec: %w", err)
	}

	return &runtimeapi.ExecSyncResponse{
		Stdout:   resp.Stdout,
		Stderr:   resp.Stderr,
		ExitCode: resp.ExitCode,
	}, nil
}

func (c *criServer) Exec(ctx context.Context, req *runtimeapi.ExecRequest) (*runtimeapi.ExecResponse, error) {
	return nil, fmt.Errorf("Exec (streaming) not implemented in v0; use ExecSync")
}

func (c *criServer) Attach(ctx context.Context, req *runtimeapi.AttachRequest) (*runtimeapi.AttachResponse, error) {
	return nil, fmt.Errorf("Attach not implemented in v0")
}

func (c *criServer) PortForward(ctx context.Context, req *runtimeapi.PortForwardRequest) (*runtimeapi.PortForwardResponse, error) {
	return nil, fmt.Errorf("PortForward not implemented in v0")
}
