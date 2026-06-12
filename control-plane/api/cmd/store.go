// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	pb "pullrun/protoapi/pullrun/control"
	rpb "pullrun/protoapi/pullrun/runtime"
)

// fileStore provides disk-backed persistence for the control plane's
// workloads and nodes maps. It is the v0 equivalent of an etcd-backed
// store — survives restarts without needing an external database.
//
// In a future phase this will be replaced by etcd for multi-node HA.
type fileStore struct {
	pb.UnimplementedControlPlaneServer

	mu          sync.RWMutex
	root        string
	workloads   map[string]*WorkloadRecord
	nodes       map[string]*NodeRecord
	runtimeConn *grpc.ClientConn // optional runtime connection
}

// workloadCheckpoint is the serializable form of WorkloadRecord.
type workloadCheckpoint struct {
	ID         string            `json:"id"`
	Name       string            `json:"name"`
	ImageRef   string            `json:"image_ref"`
	Backend    string            `json:"backend"`
	CPUMillis  uint64            `json:"cpu_millis"`
	MemoryB    uint64            `json:"memory_b"`
	Labels     map[string]string `json:"labels"`
	CreatedAt  time.Time         `json:"created_at"`
	NodeID     string            `json:"node_id"`
	Status     string            `json:"status"`
}

// nodeCheckpoint is the serializable form of NodeRecord.
type nodeCheckpoint struct {
	ID                string    `json:"id"`
	Address           string    `json:"address"`
	CPUCores          uint64    `json:"cpu_cores"`
	MemoryBytes       uint64    `json:"memory_bytes"`
	AvailableBackends []string  `json:"available_backends"`
	LastHeartbeat     time.Time `json:"last_heartbeat"`
	RunningCount      uint64    `json:"running_count"`
}

func newFileStore(root string) *fileStore {
	s := &fileStore{
		root:      root,
		workloads: make(map[string]*WorkloadRecord),
		nodes:     make(map[string]*NodeRecord),
	}
	for _, d := range []string{"workloads", "nodes"} {
		if err := os.MkdirAll(filepath.Join(root, d), 0o755); err != nil {
			log.Fatalf("failed to create store dir %s: %v", d, err)
		}
	}
	s.loadAll()
	return s
}

func (s *fileStore) loadAll() {
	wlDir := filepath.Join(s.root, "workloads")
	if entries, err := os.ReadDir(wlDir); err == nil {
		for _, e := range entries {
			if e.IsDir() || filepath.Ext(e.Name()) != ".json" {
				continue
			}
			path := filepath.Join(wlDir, e.Name())
			data, err := os.ReadFile(path)
			if err != nil {
				log.Printf("warning: cannot read workload checkpoint %s: %v", path, err)
				continue
			}
			var cp workloadCheckpoint
			if err := json.Unmarshal(data, &cp); err != nil {
				log.Printf("warning: invalid workload checkpoint %s: %v", path, err)
				continue
			}
			s.workloads[cp.ID] = &WorkloadRecord{
				ID:        cp.ID,
				Name:      cp.Name,
				ImageRef:  cp.ImageRef,
				Backend:   cp.Backend,
				CPUMillis: cp.CPUMillis,
				MemoryB:   cp.MemoryB,
				Labels:    cp.Labels,
				CreatedAt: cp.CreatedAt,
				NodeID:    cp.NodeID,
				Status:    cp.Status,
			}
		}
	}
	log.Printf("recovered %d workloads from %s", len(s.workloads), wlDir)

	nDir := filepath.Join(s.root, "nodes")
	if entries, err := os.ReadDir(nDir); err == nil {
		for _, e := range entries {
			if e.IsDir() || filepath.Ext(e.Name()) != ".json" {
				continue
			}
			path := filepath.Join(nDir, e.Name())
			data, err := os.ReadFile(path)
			if err != nil {
				log.Printf("warning: cannot read node checkpoint %s: %v", path, err)
				continue
			}
			var cp nodeCheckpoint
			if err := json.Unmarshal(data, &cp); err != nil {
				log.Printf("warning: invalid node checkpoint %s: %v", path, err)
				continue
			}
			s.nodes[cp.ID] = &NodeRecord{
				ID:                cp.ID,
				Address:           cp.Address,
				CPUCores:          cp.CPUCores,
				MemoryBytes:       cp.MemoryBytes,
				AvailableBackends: cp.AvailableBackends,
				LastHeartbeat:     cp.LastHeartbeat,
				RunningCount:      cp.RunningCount,
			}
		}
	}
	log.Printf("recovered %d nodes from %s", len(s.nodes), nDir)
}

func (s *fileStore) writeWorkload(rec *WorkloadRecord) {
	cp := workloadCheckpoint{
		ID:        rec.ID,
		Name:      rec.Name,
		ImageRef:  rec.ImageRef,
		Backend:   rec.Backend,
		CPUMillis: rec.CPUMillis,
		MemoryB:   rec.MemoryB,
		Labels:    rec.Labels,
		CreatedAt: rec.CreatedAt,
		NodeID:    rec.NodeID,
		Status:    rec.Status,
	}
	data, err := json.Marshal(cp)
	if err != nil {
		log.Printf("error marshaling workload %s: %v", rec.ID, err)
		return
	}
	if err := os.WriteFile(filepath.Join(s.root, "workloads", rec.ID+".json"), data, 0o644); err != nil {
		log.Printf("error writing workload checkpoint %s: %v", rec.ID, err)
	}
}

func (s *fileStore) writeNode(rec *NodeRecord) {
	cp := nodeCheckpoint{
		ID:                rec.ID,
		Address:           rec.Address,
		CPUCores:          rec.CPUCores,
		MemoryBytes:       rec.MemoryBytes,
		AvailableBackends: rec.AvailableBackends,
		LastHeartbeat:     rec.LastHeartbeat,
		RunningCount:      rec.RunningCount,
	}
	data, err := json.Marshal(cp)
	if err != nil {
		log.Printf("error marshaling node %s: %v", rec.ID, err)
		return
	}
	if err := os.WriteFile(filepath.Join(s.root, "nodes", rec.ID+".json"), data, 0o644); err != nil {
		log.Printf("error writing node checkpoint %s: %v", rec.ID, err)
	}
}

func (s *fileStore) removeWorkload(id string) {
	delete(s.workloads, id)
	os.Remove(filepath.Join(s.root, "workloads", id+".json"))
}

func (s *fileStore) removeNode(id string) {
	delete(s.nodes, id)
	os.Remove(filepath.Join(s.root, "nodes", id+".json"))
}

// --- APIServer methods migrated to use fileStore ---

func (s *fileStore) SubmitWorkload(ctx context.Context, req *pb.WorkloadSpec) (*rpb.WorkloadStatus, error) {
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

	nodeID, err := s.scheduleWorkload(rec)
	if err != nil {
		return nil, err
	}
	rec.NodeID = nodeID
	rec.Status = "scheduled"

	s.mu.Lock()
	s.workloads[id] = rec
	s.writeWorkload(rec)
	s.mu.Unlock()

	log.Printf("workload %s (image=%s) submitted to node %s", id, req.ImageRef, nodeID)
	return &rpb.WorkloadStatus{
		Id:              id,
		State:           rec.Status,
		Backend:         rec.Backend,
		StartTime:       now.Unix(),
		NetworkIsolated: true,
	}, nil
}

func (s *fileStore) scheduleWorkload(rec *WorkloadRecord) (string, error) {
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

		score := int64(0)
		for _, other := range s.workloads {
			if other.NodeID != id {
				continue
			}
			if other.ImageRef == rec.ImageRef {
				score += 100
			}
		}
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

func (s *fileStore) GetWorkload(ctx context.Context, req *pb.GetRequest) (*rpb.WorkloadStatus, error) {
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
	}, nil
}

func (s *fileStore) ListWorkloads(ctx context.Context, req *pb.ListRequest) (*pb.WorkloadList, error) {
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

func (s *fileStore) DeleteWorkload(ctx context.Context, req *pb.DeleteRequest) (*pb.DeleteResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.removeWorkload(req.Id)
	return &pb.DeleteResponse{Success: true}, nil
}

func (s *fileStore) RegisterNode(ctx context.Context, req *pb.NodeRegistration) (*pb.RegisterResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	rec := &NodeRecord{
		ID:                req.NodeId,
		Address:           req.Address,
		CPUCores:          req.CpuCores,
		MemoryBytes:       req.MemoryBytes,
		AvailableBackends: req.AvailableBackends,
		LastHeartbeat:     time.Now(),
	}
	s.nodes[req.NodeId] = rec
	s.writeNode(rec)

	log.Printf("node %s registered at %s (backends: %v)", req.NodeId, req.Address, req.AvailableBackends)

	return &pb.RegisterResponse{
		AssignedId:          req.NodeId,
		HeartbeatIntervalMs: 10000,
	}, nil
}

func (s *fileStore) Heartbeat(ctx context.Context, req *pb.HeartbeatRequest) (*pb.HeartbeatResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	n, ok := s.nodes[req.NodeId]
	if !ok {
		return nil, status.Error(codes.NotFound, "node not registered")
	}
	n.LastHeartbeat = time.Now()
	n.RunningCount = req.RunningWorkloads
	s.writeNode(n)

	return &pb.HeartbeatResponse{Ok: true}, nil
}

func (s *fileStore) ListNodes() []*NodeRecord {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make([]*NodeRecord, 0, len(s.nodes))
	for _, n := range s.nodes {
		out = append(out, n)
	}
	return out
}

func (s *fileStore) StreamEvents(req *pb.Empty, stream pb.ControlPlane_StreamEventsServer) error {
	<-stream.Context().Done()
	return nil
}

func (s *fileStore) dispatchToNode(ctx context.Context, rec *WorkloadRecord) error {
	if s.runtimeConn == nil {
		return nil
	}
	_ = ctx
	return nil
}
