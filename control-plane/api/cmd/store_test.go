// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	pb "pullrun/protoapi/pullrun/control"
)

func TestNewFileStore_CreatesDirs(t *testing.T) {
	dir := t.TempDir()
	s := newFileStore(dir)
	if s == nil {
		t.Fatal("newFileStore returned nil")
	}
	if _, err := os.Stat(filepath.Join(dir, "workloads")); err != nil {
			t.Errorf("workloads dir not created: %v", err)
		}
		if _, err := os.Stat(filepath.Join(dir, "nodes")); err != nil {
		t.Errorf("nodes dir not created: %v", err)
	}
}

func TestSubmitGetWorkload_Roundtrip(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	_, _ = s.RegisterNode(ctx, &pb.NodeRegistration{
		NodeId:            "node-1",
		Address:           "10.0.0.1:8080",
		CpuCores:          4,
		MemoryBytes:       8192,
		AvailableBackends: []string{"container"},
	})
	_, _ = s.Heartbeat(ctx, &pb.HeartbeatRequest{
		NodeId:           "node-1",
		RunningWorkloads: 0,
	})

	resp, err := s.SubmitWorkload(ctx, &pb.WorkloadSpec{
		Name:     "test-wl",
		ImageRef: "alpine:latest",
		Backend:  "container",
		Resources: &pb.Resources{
			CpuMillicores: 500,
			MemoryBytes:   256,
		},
	})
	if err != nil {
		t.Fatalf("SubmitWorkload: %v", err)
	}
	if resp.Id == "" {
		t.Fatal("SubmitWorkload returned empty id")
	}
	if resp.State != "scheduled" {
		t.Errorf("state = %q, want scheduled", resp.State)
	}

	got, err := s.GetWorkload(ctx, &pb.GetRequest{Id: resp.Id})
	if err != nil {
		t.Fatalf("GetWorkload: %v", err)
	}
	if got.Id != resp.Id {
		t.Errorf("GetWorkload id = %q, want %q", got.Id, resp.Id)
	}
}

func TestDeleteWorkload(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	_, _ = s.RegisterNode(ctx, &pb.NodeRegistration{
		NodeId:            "node-1",
		Address:           "10.0.0.1:8080",
		CpuCores:          4,
		MemoryBytes:       8192,
		AvailableBackends: []string{"container"},
	})
	_, _ = s.Heartbeat(ctx, &pb.HeartbeatRequest{
		NodeId:           "node-1",
		RunningWorkloads: 0,
	})

	resp, err := s.SubmitWorkload(ctx, &pb.WorkloadSpec{
		Name:     "to-delete",
		ImageRef: "busybox:latest",
		Backend:  "container",
		Resources: &pb.Resources{
			CpuMillicores: 100,
			MemoryBytes:   64,
		},
	})
	if err != nil {
		t.Fatalf("SubmitWorkload: %v", err)
	}

	del, err := s.DeleteWorkload(ctx, &pb.DeleteRequest{Id: resp.Id})
	if err != nil {
		t.Fatalf("DeleteWorkload: %v", err)
	}
	if !del.Success {
		t.Error("DeleteWorkload returned success=false")
	}

	_, err = s.GetWorkload(ctx, &pb.GetRequest{Id: resp.Id})
	if err == nil {
		t.Error("GetWorkload succeeded after delete, expected error")
	}
}

func TestListWorkloads_WithLabelFilter(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	_, _ = s.RegisterNode(ctx, &pb.NodeRegistration{
		NodeId:            "node-1",
		Address:           "10.0.0.1:8080",
		CpuCores:          4,
		MemoryBytes:       8192,
		AvailableBackends: []string{"container"},
	})

	for i := 0; i < 3; i++ {
		_, _ = s.Heartbeat(ctx, &pb.HeartbeatRequest{
			NodeId:           "node-1",
			RunningWorkloads: uint64(i),
		})
		_, _ = s.SubmitWorkload(ctx, &pb.WorkloadSpec{
			Name:     "wl",
			ImageRef: "alpine:latest",
			Backend:  "container",
			Resources: &pb.Resources{
				CpuMillicores: 100,
				MemoryBytes:   64,
			},
		})
	}

	list, err := s.ListWorkloads(ctx, &pb.ListRequest{})
	if err != nil {
		t.Fatalf("ListWorkloads: %v", err)
	}
	if len(list.Items) != 3 {
		t.Errorf("ListWorkloads count = %d, want 3", len(list.Items))
	}
}

func TestRegisterNode_AndHeartbeat(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	reg, err := s.RegisterNode(ctx, &pb.NodeRegistration{
		NodeId:            "node-x",
		Address:           "10.0.0.2:8080",
		CpuCores:          8,
		MemoryBytes:       16384,
		AvailableBackends: []string{"container", "vm"},
	})
	if err != nil {
		t.Fatalf("RegisterNode: %v", err)
	}
	if reg.AssignedId != "node-x" {
		t.Errorf("assigned_id = %q, want node-x", reg.AssignedId)
	}

	hb, err := s.Heartbeat(ctx, &pb.HeartbeatRequest{
		NodeId:           "node-x",
		RunningWorkloads: 3,
		AvailableCpuMillicores: 4000,
	})
	if err != nil {
		t.Fatalf("Heartbeat: %v", err)
	}
	if !hb.Ok {
		t.Error("Heartbeat returned ok=false")
	}

	nodes := s.ListNodes()
	if len(nodes) != 1 {
		t.Fatalf("ListNodes = %d, want 1", len(nodes))
	}
	if nodes[0].RunningCount != 3 {
		t.Errorf("RunningCount = %d, want 3", nodes[0].RunningCount)
	}
}

func TestPersistence_AcrossReload(t *testing.T) {
	dir := t.TempDir()
	ctx := context.Background()

	s1 := newFileStore(dir)
	_, _ = s1.RegisterNode(ctx, &pb.NodeRegistration{
		NodeId:            "node-p",
		Address:           "10.0.0.3:8080",
		CpuCores:          2,
		MemoryBytes:       4096,
		AvailableBackends: []string{"container"},
	})
	_, _ = s1.Heartbeat(ctx, &pb.HeartbeatRequest{
		NodeId:           "node-p",
		RunningWorkloads: 1,
	})
	_, _ = s1.SubmitWorkload(ctx, &pb.WorkloadSpec{
		Name:     "persist-wl",
		ImageRef: "nginx:latest",
		Backend:  "container",
		Resources: &pb.Resources{
			CpuMillicores: 100,
			MemoryBytes:   64,
		},
	})

	// Reload
	s2 := newFileStore(dir)
	list, err := s2.ListWorkloads(ctx, &pb.ListRequest{})
	if err != nil {
		t.Fatalf("ListWorkloads after reload: %v", err)
	}
	if len(list.Items) != 1 {
		t.Fatalf("workloads after reload = %d, want 1", len(list.Items))
	}
	if list.Items[0].Id == "" {
		t.Error("workload id is empty after reload")
	}

	nodes := s2.ListNodes()
	if len(nodes) != 1 {
		t.Fatalf("nodes after reload = %d, want 1", len(nodes))
	}
}

func TestScheduleWorkload_NoHealthyNodes(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	// Submit without any nodes registered
	_, err := s.SubmitWorkload(ctx, &pb.WorkloadSpec{
		Name:     "nowhere",
		ImageRef: "alpine:latest",
		Backend:  "container",
		Resources: &pb.Resources{
			CpuMillicores: 100,
			MemoryBytes:   64,
		},
	})
	if err == nil {
		t.Fatal("SubmitWorkload succeeded without any node, expected error")
	}
}

func TestHeartbeat_UnknownNode(t *testing.T) {
	s := newFileStore(t.TempDir())
	ctx := context.Background()

	_, err := s.Heartbeat(ctx, &pb.HeartbeatRequest{
		NodeId:           "ghost",
		RunningWorkloads: 1,
	})
	if err == nil {
		t.Error("Heartbeat for unknown node should fail")
	}
}

func TestListNodes_Empty(t *testing.T) {
	s := newFileStore(t.TempDir())
	nodes := s.ListNodes()
	if len(nodes) != 0 {
		t.Errorf("ListNodes on empty store = %d, want 0", len(nodes))
	}
}
