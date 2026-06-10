// Package main implements the Nimbus CRI shim.
//
// Maps the Kubernetes Container Runtime Interface (CRI) onto the nimbus-runtime
// gRPC service. A "pod sandbox" becomes one Nimbus workload (container or VM
// depending on the RuntimeHandler / RuntimeClass); containers are stubbed as
// 1:1 with sandboxes (true pod-in-VM is a Phase 4+ enhancement).
package main

import (
	"context"
	"flag"
	"log"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"sync"
	"syscall"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"

	nimbusruntime "nimbus/protoapi/nimbus/runtime"
)

const (
	// NimbusContainerRuntimeClass is the RuntimeClass name for container workloads.
	NimbusContainerRuntimeClass = "nimbus-container"
	// NimbusVMRuntimeClass is the RuntimeClass name for VM workloads.
	NimbusVMRuntimeClass = "nimbus-vm"
	// NimbusCRIVersion is the reported CRI version.
	NimbusCRIVersion = "0.1.0"
	// DefaultPauseImage is the standard K8s sandbox image (no-op container).
	DefaultPauseImage = "registry.k8s.io/pause:3.9"
	// AnnotationNimbusImage overrides the image to run in a pod sandbox.
	AnnotationNimbusImage = "nimbus.io/image"
	// AnnotationNimbusCPUMillicores overrides the CPU resource limit.
	AnnotationNimbusCPUMillicores = "nimbus.io/cpu-millicores"
	// AnnotationNimbusMemoryBytes overrides the memory resource limit.
	AnnotationNimbusMemoryBytes = "nimbus.io/memory-bytes"
)

// criServer is the gRPC server implementation that bridges CRI to nimbus-runtime.
type criServer struct {
	runtimeapi.UnimplementedRuntimeServiceServer
	runtimeapi.UnimplementedImageServiceServer

	runtimeClient nimbusruntime.RuntimeClient
	sandboxStore  *fileStore
	streaming     *streamingServer
	networkMode   string
}

// sandboxStore is a small in-memory index of pod sandboxes -> nimbus workload IDs.
// It is the bare minimum to make ListPodSandbox and ContainerStatus work in
// single-node mode. A real implementation would query the control plane.
type sandboxStore struct {
	mu        sync.RWMutex
	sandboxes map[string]*sandboxRecord
	containers map[string]*containerRecord // containerID -> record
}

type sandboxRecord struct {
	id           string
	nimbusID     string
	namespace    string
	name         string
	createdAt    time.Time
	state        runtimeapi.PodSandboxState
	internalIP   string
	runtimeClass string
}

type containerRecord struct {
	id         string
	sandboxID  string
	nimbusID   string
	name       string
	image      string
	createdAt  time.Time
}

func newSandboxStore() *sandboxStore {
	return &sandboxStore{
		sandboxes:  make(map[string]*sandboxRecord),
		containers: make(map[string]*containerRecord),
	}
}

func (s *sandboxStore) putSandbox(rec *sandboxRecord) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.sandboxes[rec.id] = rec
}

func (s *sandboxStore) getSandbox(id string) (*sandboxRecord, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rec, ok := s.sandboxes[id]
	return rec, ok
}

func (s *sandboxStore) listSandboxes(filter *runtimeapi.PodSandboxFilter) []*runtimeapi.PodSandbox {
	s.mu.RLock()
	defer s.mu.RUnlock()

	out := make([]*runtimeapi.PodSandbox, 0, len(s.sandboxes))
	for _, rec := range s.sandboxes {
		if !matchesSandboxFilter(rec, filter) {
			continue
		}
		out = append(out, sandboxToAPI(rec))
	}
	return out
}

func (s *sandboxStore) removeSandbox(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.sandboxes, id)
	// Cascade-delete containers belonging to this sandbox
	for cid, c := range s.containers {
		if c.sandboxID == id {
			delete(s.containers, cid)
		}
	}
}

func (s *sandboxStore) putContainer(rec *containerRecord) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.containers[rec.id] = rec
}

func (s *sandboxStore) getContainer(id string) (*containerRecord, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rec, ok := s.containers[id]
	return rec, ok
}

func (s *sandboxStore) listContainers(filter *runtimeapi.ContainerFilter) []*runtimeapi.Container {
	s.mu.RLock()
	defer s.mu.RUnlock()

	out := make([]*runtimeapi.Container, 0, len(s.containers))
	for _, rec := range s.containers {
		if filter != nil && filter.PodSandboxId != "" && rec.sandboxID != filter.PodSandboxId {
			continue
		}
		out = append(out, containerToAPI(rec))
	}
	return out
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
		// PodSandbox doesn't carry arbitrary labels in v0, so accept all
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
		// Network is in PodSandboxStatus, not PodSandbox, in CRI v1.
	}
}

func containerToAPI(rec *containerRecord) *runtimeapi.Container {
	return &runtimeapi.Container{
		Id:           rec.id,
		PodSandboxId: rec.sandboxID,
		Metadata:     &runtimeapi.ContainerMetadata{Name: rec.name},
		Image:        &runtimeapi.ImageSpec{Image: rec.image},
		CreatedAt:    rec.createdAt.UnixNano(),
		State:        runtimeapi.ContainerState_CONTAINER_CREATED,
	}
}

func main() {
	var (
		socketPath   = flag.String("socket", "/var/run/nimbus/nimbus-cri.sock", "CRI socket path")
		runtimeSock  = flag.String("runtime-socket", "/var/run/nimbus/runtime.sock", "nimbus-runtime gRPC UDS")
		networkMode  = flag.String("network-mode", "bridge", "Network mode for workloads: 'isolated' (no cluster IP) or 'bridge' (shared nimbus-br0)")
	)
	flag.Parse()

	if err := os.RemoveAll(*socketPath); err != nil {
		log.Fatalf("failed to remove old CRI socket: %v", err)
	}
	if err := os.MkdirAll(filepath.Dir(*socketPath), 0o755); err != nil {
		log.Fatalf("failed to create socket dir: %v", err)
	}

	// Connect to the nimbus-runtime service over UDS.
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	conn, err := grpc.DialContext(
		ctx,
		"unix://"+*runtimeSock,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithBlock(),
	)
	if err != nil {
		log.Fatalf("failed to connect to nimbus-runtime at %s: %v", *runtimeSock, err)
	}
	defer conn.Close()
	log.Printf("connected to nimbus-runtime at %s", *runtimeSock)

	runtimeClient := nimbusruntime.NewRuntimeClient(conn)

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

	log.Printf("Nimbus CRI shim v%s listening on %s", NimbusCRIVersion, *socketPath)
	log.Printf("supported RuntimeClasses: %s, %s", NimbusContainerRuntimeClass, NimbusVMRuntimeClass)

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
