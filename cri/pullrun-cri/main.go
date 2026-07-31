// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

// Package main implements the Pullrun CRI shim.
//
// Maps the Kubernetes Container Runtime Interface (CRI) onto the pullrun-runtime
// gRPC service. A "pod sandbox" becomes one Pullrun workload (container or VM
// depending on the RuntimeHandler / RuntimeClass) that anchors the pod's
// network namespace; every CRI container is a real Pullrun workload that runs
// its own image and joins the sandbox's network namespace (container backend)
// or runs as an independent bridged workload (VM backend).
package main

import (
	"context"
	"flag"
	"log"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"

	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

const (
	// PullrunContainerRuntimeClass is the RuntimeClass name for container workloads.
	PullrunContainerRuntimeClass = "pullrun-container"
	// PullrunVMRuntimeClass is the RuntimeClass name for VM workloads.
	PullrunVMRuntimeClass = "pullrun-vm"
	// PullrunCRIVersion is the CRI implementation version reported to kubelet.
	PullrunCRIVersion = "1.0.0"
	// DefaultPauseImage is the standard K8s sandbox image (no-op container).
	DefaultPauseImage = "registry.k8s.io/pause:3.9"
	// AnnotationPullrunImage overrides the image to run in a pod sandbox.
	AnnotationPullrunImage = "pullrun.io/image"
	// AnnotationPullrunCPUMillicores overrides the CPU resource limit.
	AnnotationPullrunCPUMillicores = "pullrun.io/cpu-millicores"
	// AnnotationPullrunMemoryBytes overrides the memory resource limit.
	AnnotationPullrunMemoryBytes = "pullrun.io/memory-bytes"
)

// criServer is the gRPC server implementation that bridges CRI to pullrun-runtime.
type criServer struct {
	runtimeapi.UnimplementedRuntimeServiceServer
	runtimeapi.UnimplementedImageServiceServer

	runtimeClient pullrunruntime.RuntimeClient
	sandboxStore  *fileStore
	streaming     *streamingServer
	networkMode   string
}

type sandboxRecord struct {
	id           string
	pullrunID    string
	namespace    string
	name         string
	createdAt    time.Time
	state        runtimeapi.PodSandboxState
	internalIP   string
	runtimeClass string
	bridgeName   string
	hostNetwork  bool
	dns          *pullrunruntime.DnsConfig
}

type containerRecord struct {
	id        string
	sandboxID string
	pullrunID string
	name      string
	image     string
	imageRef  string
	createdAt time.Time
	startedAt time.Time
	state     runtimeapi.ContainerState
	exitCode  int32
}

func matchesSandboxFilter(rec *sandboxRecord, filter *runtimeapi.PodSandboxFilter) bool {
	if filter == nil {
		return true
	}
	if filter.Id != "" && filter.Id != rec.id {
		return false
	}
	if filter.State != nil && filter.State.State != rec.state {
		return false
	}
	if filter.LabelSelector != nil {
		_ = filter.LabelSelector
	}
	return true
}

func sandboxToAPI(rec *sandboxRecord) *runtimeapi.PodSandbox {
	return &runtimeapi.PodSandbox{
		Id:             rec.id,
		Metadata:       &runtimeapi.PodSandboxMetadata{Name: rec.name, Namespace: rec.namespace, Uid: rec.id},
		State:          rec.state,
		CreatedAt:      rec.createdAt.UnixNano(),
		RuntimeHandler: rec.runtimeClass,
	}
}

func containerToAPI(rec *containerRecord) *runtimeapi.Container {
	return &runtimeapi.Container{
		Id:           rec.id,
		PodSandboxId: rec.sandboxID,
		Metadata:     &runtimeapi.ContainerMetadata{Name: rec.name},
		Image:        &runtimeapi.ImageSpec{Image: rec.image},
		ImageRef:     rec.imageRef,
		CreatedAt:    rec.createdAt.UnixNano(),
		State:        rec.state,
	}
}

// warmUpPauseImage pre-pulls the pause image so the first RunPodSandbox
// is a store hit instead of a network pull.
func (c *criServer) warmUpPauseImage() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	hasCtx, hasCancel := context.WithTimeout(ctx, 5*time.Second)
	resp, err := c.runtimeClient.HasImage(hasCtx, &pullrunruntime.HasImageRequest{ImageRef: DefaultPauseImage})
	hasCancel()
	if err == nil && resp.Exists {
		log.Printf("pause image already cached (root %s)", resp.RootDigest)
		return
	}
	start := time.Now()
	if _, err := c.ensureImage(ctx, DefaultPauseImage, ""); err != nil {
		log.Printf("warning: pause image warm-up failed: %v", err)
		return
	}
	log.Printf("pause image warmed up in %s", time.Since(start).Round(time.Millisecond))
}

func main() {
	var (
		socketPath  = flag.String("socket", "/var/run/pullrun/pullrun-cri.sock", "CRI socket path")
		runtimeSock = flag.String("runtime-socket", "/var/run/pullrun/runtime.sock", "pullrun-runtime gRPC UDS")
		networkMode = flag.String("network-mode", "bridge", "Network mode: bridge|isolated|host|slirp")
	)
	flag.Parse()

	if err := os.RemoveAll(*socketPath); err != nil {
		log.Fatalf("failed to remove old CRI socket: %v", err)
	}
	if err := os.MkdirAll(filepath.Dir(*socketPath), 0o755); err != nil {
		log.Fatalf("failed to create socket dir: %v", err)
	}

	// Connect to the pullrun-runtime service over UDS.
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	conn, err := grpc.DialContext(
		ctx,
		"unix://"+*runtimeSock,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithBlock(),
	)
	if err != nil {
		log.Fatalf("failed to connect to pullrun-runtime at %s: %v", *runtimeSock, err)
	}
	defer conn.Close()
	log.Printf("connected to pullrun-runtime at %s", *runtimeSock)

	runtimeClient := pullrunruntime.NewRuntimeClient(conn)

	// Start the SPDY streaming server for Exec/Attach/PortForward.
	streaming, err := newStreamingServer(runtimeClient)
	if err != nil {
		log.Fatalf("failed to start streaming server: %v", err)
	}

	server := &criServer{
		runtimeClient: runtimeClient,
		sandboxStore:  newFileStore(filepath.Join(filepath.Dir(*socketPath), "store")),
		streaming:     streaming,
		networkMode:   *networkMode,
	}

	// Reconcile the local store against the runtime's live workloads so
	// pods/containers that died while the shim was down are reported as
	// exited instead of phantom-running.
	server.reconcileStartup(ctx)

	// Warm up the pause image in the background so the first pod
	// creation is fast; failures are logged, not fatal.
	go server.warmUpPauseImage()
	// Listen on the CRI socket
	lis, err := net.Listen("unix", *socketPath)
	if err != nil {
		log.Fatalf("failed to listen on %s: %v", *socketPath, err)
	}
	if err := os.Chmod(*socketPath, 0o660); err != nil {
		log.Printf("warning: could not chmod CRI socket: %v", err)
	}

	gs := grpc.NewServer()
	runtimeapi.RegisterRuntimeServiceServer(gs, server)
	runtimeapi.RegisterImageServiceServer(gs, server)

	log.Printf("Pullrun CRI shim v%s listening on %s", PullrunCRIVersion, *socketPath)
	log.Printf("supported RuntimeClasses: %s, %s", PullrunContainerRuntimeClass, PullrunVMRuntimeClass)

	// Graceful shutdown on SIGTERM / SIGINT.
	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT)
		s := <-sig
		log.Printf("received %v, shutting down gracefully", s)
		gs.GracefulStop()
	}()

	if err := gs.Serve(lis); err != nil {
		log.Fatalf("CRI server error: %v", err)
	}
}
