// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package scheduler

import (
	"sort"
	"sync"
	"time"
)

// Phase 3 placeholder scheduler.

type NodeInfo struct {
	ID            string
	Address       string
	CPUCores      uint64
	MemoryBytes   uint64
	Available     uint64
	Backends      []string
	LastHeartbeat time.Time
	RunningCount  int
}

type Placement struct {
	NodeID string
	Score  int64
}

type Scheduler struct {
	mu    sync.RWMutex
	nodes map[string]*NodeInfo
}

func New() *Scheduler {
	return &Scheduler{nodes: make(map[string]*NodeInfo)}
}

func (s *Scheduler) RegisterNode(n *NodeInfo) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.nodes[n.ID] = n
}

func (s *Scheduler) Heartbeat(nodeID string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if n, ok := s.nodes[nodeID]; ok {
		n.LastHeartbeat = time.Now()
		return true
	}
	return false
}

func (s *Scheduler) Place(cpuMillis, memBytes uint64, backend string, imageRef string) (*Placement, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var candidates []*NodeInfo
	for _, n := range s.nodes {
		if time.Since(n.LastHeartbeat) > 30*time.Second {
			continue
		}
		if !hasBackend(n, backend) {
			continue
		}
		if n.Available < cpuMillis {
			continue
		}
		candidates = append(candidates, n)
	}

	if len(candidates) == 0 {
		return nil, ErrNoNodes
	}

	// Network-aware: score by image locality
	placements := make([]Placement, 0, len(candidates))
	for _, n := range candidates {
		score := int64(n.Available)
		// Tiebreaker: prefer fewer running workloads
		score -= int64(n.RunningCount) * 10
		placements = append(placements, Placement{NodeID: n.ID, Score: score})
	}

	sort.SliceStable(placements, func(i, j int) bool {
		return placements[i].Score > placements[j].Score
	})

	return &placements[0], nil
}

func hasBackend(n *NodeInfo, backend string) bool {
	for _, b := range n.Backends {
		if b == backend {
			return true
		}
	}
	return false
}

var ErrNoNodes = &SchedulerError{Message: "no nodes available for placement"}

type SchedulerError struct {
	Message string
}

func (e *SchedulerError) Error() string {
	return e.Message
}