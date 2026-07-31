// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"log"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

// ============================================================
// Stats service — ContainerStats / ListContainerStats /
// PodSandboxStats / ListPodSandboxStats backed by the runtime's
// GetWorkloadStats.
// ============================================================

// workloadStats fetches live stats for a workload, returning nil on error.
func (c *criServer) workloadStats(ctx context.Context, workloadID string) *pullrunruntime.WorkloadStats {
	statsCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	stats, err := c.runtimeClient.GetWorkloadStats(statsCtx, &pullrunruntime.GetWorkloadStatsRequest{Id: workloadID})
	cancel()
	if err != nil {
		log.Printf("GetWorkloadStats id=%s (runtime error: %v)", workloadID, err)
		return nil
	}
	return stats
}

// containerStats builds the CRI ContainerStats from runtime stats.
func containerStats(rec *containerRecord, s *pullrunruntime.WorkloadStats) *runtimeapi.ContainerStats {
	now := time.Now().UnixNano()
	stats := &runtimeapi.ContainerStats{
		Attributes: &runtimeapi.ContainerAttributes{
			Id:       rec.id,
			Metadata: &runtimeapi.ContainerMetadata{Name: rec.name},
			Labels:   map[string]string{"io.kubernetes.pod.sandbox.id": rec.sandboxID},
		},
		WritableLayer: &runtimeapi.FilesystemUsage{
			Timestamp: now,
			FsId:      &runtimeapi.FilesystemIdentifier{Mountpoint: "dag"},
		},
	}
	if s != nil {
		stats.Cpu = &runtimeapi.CpuUsage{
			Timestamp:            now,
			UsageNanoCores:       &runtimeapi.UInt64Value{Value: uint64(s.CpuUsagePercent * 10_000_000)}, // percent of one core
			UsageCoreNanoSeconds: &runtimeapi.UInt64Value{},
		}
		stats.Memory = &runtimeapi.MemoryUsage{
			Timestamp:       now,
			UsageBytes:      &runtimeapi.UInt64Value{Value: s.MemoryBytes},
			WorkingSetBytes: &runtimeapi.UInt64Value{Value: s.MemoryBytes},
			AvailableBytes:  &runtimeapi.UInt64Value{},
		}
		stats.WritableLayer.UsedBytes = &runtimeapi.UInt64Value{Value: s.DiskBytes}
	}
	return stats
}

func (c *criServer) ContainerStats(ctx context.Context, req *runtimeapi.ContainerStatsRequest) (*runtimeapi.ContainerStatsResponse, error) {
	rec, ok := c.sandboxStore.getContainer(req.ContainerId)
	if !ok {
		return nil, fmt.Errorf("ContainerStats: container %q not found", req.ContainerId)
	}
	stats := containerStats(rec, c.workloadStats(ctx, rec.pullrunID))
	return &runtimeapi.ContainerStatsResponse{Stats: stats}, nil
}

func (c *criServer) ListContainerStats(ctx context.Context, req *runtimeapi.ListContainerStatsRequest) (*runtimeapi.ListContainerStatsResponse, error) {
	filter := req.Filter
	var recs []*containerRecord
	c.sandboxStore.mu.RLock()
	for _, rec := range c.sandboxStore.containers {
		if filter != nil && filter.Id != "" && rec.id != filter.Id {
			continue
		}
		if filter != nil && filter.PodSandboxId != "" && rec.sandboxID != filter.PodSandboxId {
			continue
		}
		recs = append(recs, rec)
	}
	c.sandboxStore.mu.RUnlock()

	out := make([]*runtimeapi.ContainerStats, 0, len(recs))
	for _, rec := range recs {
		out = append(out, containerStats(rec, c.workloadStats(ctx, rec.pullrunID)))
	}
	return &runtimeapi.ListContainerStatsResponse{Stats: out}, nil
}

func (c *criServer) PodSandboxStats(ctx context.Context, req *runtimeapi.PodSandboxStatsRequest) (*runtimeapi.PodSandboxStatsResponse, error) {
	rec, ok := c.sandboxStore.getSandbox(req.PodSandboxId)
	if !ok {
		return nil, fmt.Errorf("PodSandboxStats: sandbox %q not found", req.PodSandboxId)
	}

	// Sandbox (pause) workload stats + every container in the pod.
	sandboxStats := c.workloadStats(ctx, rec.pullrunID)
	var containerStatsList []*runtimeapi.ContainerStats
	for _, crec := range c.sandboxStore.allContainersForSandbox(req.PodSandboxId) {
		containerStatsList = append(containerStatsList, containerStats(crec, c.workloadStats(ctx, crec.pullrunID)))
	}

	now := time.Now().UnixNano()
	podStats := &runtimeapi.PodSandboxStats{
		Attributes: &runtimeapi.PodSandboxAttributes{
			Id:       rec.id,
			Metadata: &runtimeapi.PodSandboxMetadata{Name: rec.name, Namespace: rec.namespace, Uid: rec.id},
		},
		Linux: &runtimeapi.LinuxPodSandboxStats{
			Containers: containerStatsList,
		},
	}
	if sandboxStats != nil {
		podStats.Linux.Cpu = &runtimeapi.CpuUsage{
			Timestamp:            now,
			UsageNanoCores:       &runtimeapi.UInt64Value{Value: uint64(sandboxStats.CpuUsagePercent * 10_000_000)},
			UsageCoreNanoSeconds: &runtimeapi.UInt64Value{},
		}
		podStats.Linux.Memory = &runtimeapi.MemoryUsage{
			Timestamp:       now,
			UsageBytes:      &runtimeapi.UInt64Value{Value: sandboxStats.MemoryBytes},
			WorkingSetBytes: &runtimeapi.UInt64Value{Value: sandboxStats.MemoryBytes},
		}
		podStats.Linux.Network = &runtimeapi.NetworkUsage{
			Timestamp: now,
			DefaultInterface: &runtimeapi.NetworkInterfaceUsage{
				Name:     "eth0",
				RxBytes:  &runtimeapi.UInt64Value{Value: sandboxStats.NetworkRxBytes},
				TxBytes:  &runtimeapi.UInt64Value{Value: sandboxStats.NetworkTxBytes},
				RxErrors: &runtimeapi.UInt64Value{},
				TxErrors: &runtimeapi.UInt64Value{},
			},
		}
	}

	return &runtimeapi.PodSandboxStatsResponse{Stats: podStats}, nil
}

func (c *criServer) ListPodSandboxStats(ctx context.Context, req *runtimeapi.ListPodSandboxStatsRequest) (*runtimeapi.ListPodSandboxStatsResponse, error) {
	filter := req.Filter
	var sandboxes []*sandboxRecord
	c.sandboxStore.mu.RLock()
	for _, rec := range c.sandboxStore.sandboxes {
		if filter != nil && filter.Id != "" && rec.id != filter.Id {
			continue
		}
		sandboxes = append(sandboxes, rec)
	}
	c.sandboxStore.mu.RUnlock()

	out := make([]*runtimeapi.PodSandboxStats, 0, len(sandboxes))
	for _, rec := range sandboxes {
		resp, err := c.PodSandboxStats(ctx, &runtimeapi.PodSandboxStatsRequest{PodSandboxId: rec.id})
		if err != nil {
			log.Printf("PodSandboxStats sandbox=%s (error: %v)", rec.id, err)
			continue
		}
		out = append(out, resp.Stats)
	}
	return &runtimeapi.ListPodSandboxStatsResponse{Stats: out}, nil
}
