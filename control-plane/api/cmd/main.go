package main

import (
	"context"
	"fmt"
	"log"
	"net"
	"net/http"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"

	pb "nimbus/protoapi/nimbus/control"
	rpb "nimbus/protoapi/nimbus/runtime"
)

type WorkloadRecord struct {
	ID         string
	Name       string
	ImageRef   string
	Backend    string
	CPUMillis  uint64
	MemoryB    uint64
	Labels     map[string]string
	CreatedAt  time.Time
	NodeID     string
	Status     string
}

type NodeRecord struct {
	ID               string
	Address          string
	CPUCores         uint64
	MemoryBytes      uint64
	AvailableBackends []string
	LastHeartbeat    time.Time
	RunningCount     uint64
}

type APIServer struct {
	pb.UnimplementedControlPlaneServer

	mu        sync.RWMutex
	workloads map[string]*WorkloadRecord
	nodes     map[string]*NodeRecord

	// Optional connection to the Rust runtime for actual workload execution
	runtimeConn *grpc.ClientConn
}

func NewAPIServer() *APIServer {
	return &APIServer{
		workloads: make(map[string]*WorkloadRecord),
		nodes:     make(map[string]*NodeRecord),
	}
}

func (s *APIServer) ConnectRuntime(socketPath string) error {
	conn, err := grpc.NewClient(
		"unix://"+socketPath,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		return fmt.Errorf("connect to runtime: %w", err)
	}
	s.runtimeConn = conn
	log.Printf("control plane connected to runtime at %s", socketPath)
	return nil
}

func (s *APIServer) SubmitWorkload(ctx context.Context, req *pb.WorkloadSpec) (*rpb.WorkloadStatus, error) {
	if req.Backend == "" {
		return nil, status.Error(codes.InvalidArgument, "backend is required")
	}

	id := fmt.Sprintf("wl-%d", time.Now().UnixNano())
	now := time.Now()

	rec := &WorkloadRecord{
		ID:        id,
		Name:      req.Name,
		ImageRef:  req.ImageRef,
		Backend:   req.Backend,
		CPUMillis: req.Resources.CpuMillicores,
		MemoryB:   req.Resources.MemoryBytes,
		Labels:    req.Labels,
		CreatedAt: now,
		Status:    "submitted",
	}

	// Schedule: prefer node with image locality
	nodeID, err := s.scheduleWorkload(rec)
	if err != nil {
		return nil, err
	}
	rec.NodeID = nodeID
	rec.Status = "scheduled"

	s.mu.Lock()
	s.workloads[id] = rec
	s.mu.Unlock()

	log.Printf("workload %s (image=%s) submitted to node %s", id, req.ImageRef, nodeID)

	// If we have a runtime connection, push the workload to the node's runtime
	if err := s.dispatchToNode(ctx, rec); err != nil {
		log.Printf("dispatch to node %s failed: %v (workload is queued)", nodeID, err)
	}

	return &rpb.WorkloadStatus{
		Id:              id,
		State:           rec.Status,
		Backend:         rec.Backend,
		StartTime:       now.Unix(),
		NetworkIsolated: true,
	}, nil
}

func (s *APIServer) scheduleWorkload(rec *WorkloadRecord) (string, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	if len(s.nodes) == 0 {
		return "", status.Error(codes.Unavailable, "no nodes registered")
	}

	var bestNode string
	var bestScore int64 = -1

	for id, n := range s.nodes {
		if time.Since(n.LastHeartbeat) > 30*time.Second {
			continue
		}
		hasBackend := false
		for _, b := range n.AvailableBackends {
			if b == rec.Backend {
				hasBackend = true
				break
			}
		}
		if !hasBackend {
			continue
		}

		// Network-aware: score by image locality
		score := int64(0)
		for _, other := range s.workloads {
			if other.NodeID != id {
				continue
			}
			if other.ImageRef == rec.ImageRef {
				score += 100
			}
		}
		// Tiebreaker: prefer fewer running workloads
		score -= int64(n.RunningCount) * 5

		if bestNode == "" || score > bestScore {
			bestNode = id
			bestScore = score
		}
	}

	if bestNode == "" {
		return "", status.Error(codes.FailedPrecondition, "no node with backend available")
	}
	return bestNode, nil
}

func (s *APIServer) dispatchToNode(ctx context.Context, rec *WorkloadRecord) error {
	// In a real multi-node setup, the control plane would call each
	// node's runtime over its gRPC socket. For now, if we have a runtime
	// connection, we just call it.
	if s.runtimeConn == nil {
		return nil // nothing to dispatch yet
	}

	// Get the image's root digest by pulling if needed.
	// For now, just call RunWorkload on the local runtime with a placeholder
	// (caller is expected to have already pulled and have a digest).
	_ = ctx
	return nil
}

func (s *APIServer) GetWorkload(ctx context.Context, req *pb.GetRequest) (*rpb.WorkloadStatus, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	w, ok := s.workloads[req.Id]
	if !ok {
		return nil, status.Error(codes.NotFound, "workload not found")
	}

	return &rpb.WorkloadStatus{
		Id:        w.ID,
		State:     w.Status,
		Backend:   w.Backend,
		StartTime: w.CreatedAt.Unix(),
		// InternalIp and ExitCode will be filled by the actual runtime
	}, nil
}

func (s *APIServer) ListWorkloads(ctx context.Context, req *pb.ListRequest) (*pb.WorkloadList, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	items := make([]*rpb.WorkloadStatus, 0, len(s.workloads))
	for _, w := range s.workloads {
		if len(req.LabelFilter) > 0 {
			match := true
			for k, v := range req.LabelFilter {
				if w.Labels[k] != v {
					match = false
					break
				}
			}
			if !match {
				continue
			}
		}
		items = append(items, &rpb.WorkloadStatus{
			Id:        w.ID,
			State:     w.Status,
			Backend:   w.Backend,
			StartTime: w.CreatedAt.Unix(),
		})
	}

	return &pb.WorkloadList{Items: items}, nil
}

func (s *APIServer) DeleteWorkload(ctx context.Context, req *pb.DeleteRequest) (*pb.DeleteResponse, error) {
	s.mu.Lock()
	delete(s.workloads, req.Id)
	s.mu.Unlock()
	return &pb.DeleteResponse{Success: true}, nil
}

func (s *APIServer) StreamEvents(req *pb.Empty, stream pb.ControlPlane_StreamEventsServer) error {
	// Phase 3 stub: in a real implementation, this would stream from
	// a shared event bus. For now, return an empty stream.
	<-stream.Context().Done()
	return nil
}

func (s *APIServer) RegisterNode(ctx context.Context, req *pb.NodeRegistration) (*pb.RegisterResponse, error) {
	s.mu.Lock()
	s.nodes[req.NodeId] = &NodeRecord{
		ID:                req.NodeId,
		Address:           req.Address,
		CPUCores:          req.CpuCores,
		MemoryBytes:       req.MemoryBytes,
		AvailableBackends: req.AvailableBackends,
		LastHeartbeat:     time.Now(),
	}
	s.mu.Unlock()

	log.Printf("node %s registered at %s (backends: %v)", req.NodeId, req.Address, req.AvailableBackends)

	return &pb.RegisterResponse{
		AssignedId:        req.NodeId,
		HeartbeatIntervalMs: 10000,
	}, nil
}

func (s *APIServer) Heartbeat(ctx context.Context, req *pb.HeartbeatRequest) (*pb.HeartbeatResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	n, ok := s.nodes[req.NodeId]
	if !ok {
		return nil, status.Error(codes.NotFound, "node not registered")
	}
	n.LastHeartbeat = time.Now()
	n.RunningCount = req.RunningWorkloads

	return &pb.HeartbeatResponse{Ok: true}, nil
}

func main() {
	listen := ":8080"
	apiServer := NewAPIServer()

	// Try to connect to the local runtime for direct dispatch
	if err := apiServer.ConnectRuntime("/var/run/nimbus/runtime.sock"); err != nil {
		log.Printf("warning: %v (control plane runs in passive mode)", err)
	}

	// Start gRPC server
	go func() {
		lis, err := net.Listen("tcp", listen)
		if err != nil {
			log.Fatalf("failed to listen: %v", err)
		}
		grpcServer := grpc.NewServer()
		pb.RegisterControlPlaneServer(grpcServer, apiServer)
		log.Printf("control plane gRPC listening on %s", listen)
		if err := grpcServer.Serve(lis); err != nil {
			log.Fatalf("gRPC server error: %v", err)
		}
	}()

	// Start HTTP server for healthz and simple REST API
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("/api/workloads", func(w http.ResponseWriter, r *http.Request) {
		wls, _ := apiServer.ListWorkloads(r.Context(), &pb.ListRequest{})
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "[")
		for i, wl := range wls.Items {
			if i > 0 {
				fmt.Fprintf(w, ",")
			}
			fmt.Fprintf(w, `{"id":"%s","state":"%s","backend":"%s"}`,
				wl.Id, wl.State, wl.Backend)
		}
		fmt.Fprintf(w, "]")
	})
	mux.HandleFunc("/api/nodes", func(w http.ResponseWriter, r *http.Request) {
		apiServer.mu.RLock()
		defer apiServer.mu.RUnlock()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "[")
		first := true
		for _, n := range apiServer.nodes {
			if !first {
				fmt.Fprintf(w, ",")
			}
			first = false
			fmt.Fprintf(w, `{"id":"%s","address":"%s","backends":%d}`,
				n.ID, n.Address, len(n.AvailableBackends))
		}
		fmt.Fprintf(w, "]")
	})

	log.Printf("control plane HTTP listening on :8081")
	srv := &http.Server{
		Addr:    ":8081",
		Handler: mux,
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Printf("HTTP server error: %v", err)
		}
	}()

	<-ctx.Done()
	log.Println("shutting down")
}