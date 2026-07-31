// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"strings"
	"testing"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
)

func TestContainerWorkloadID(t *testing.T) {
	// Long hex ids get truncated to the stable tail.
	long := strings.Repeat("a", 64)
	got := containerWorkloadID(long)
	if !strings.HasPrefix(got, "c-") {
		t.Errorf("workload id %q must start with c-", got)
	}
	if len(got) > 22 { // "c-" + 20 chars
		t.Errorf("workload id %q too long", got)
	}
	// Ids must match the runtime's [a-zA-Z0-9._-] rule.
	for _, c := range got {
		if !(c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '-') {
			t.Errorf("workload id %q contains invalid char %q", got, c)
		}
	}
	// Deterministic.
	if containerWorkloadID(long) != containerWorkloadID(long) {
		t.Error("workload id must be deterministic")
	}
}

func TestPodBridgeName(t *testing.T) {
	id := "8f2c4d1a-3b5e-4c7d-9a0f-1b2c3d4e5f6a"
	got := podBridgeName(id)
	if len(got) > 15 {
		t.Errorf("bridge name %q exceeds IFNAMSIZ (15 chars)", got)
	}
	if !strings.HasPrefix(got, "pr-") {
		t.Errorf("bridge name %q must start with pr-", got)
	}
	// Deterministic + distinct for different pod ids.
	if podBridgeName(id) != podBridgeName(id) {
		t.Error("bridge name must be deterministic")
	}
	if podBridgeName(id) == podBridgeName(strings.Repeat("b", 36)) {
		t.Error("different pods must get different bridge names")
	}
}

func TestSeccompProfileFor(t *testing.T) {
	if got := seccompProfileFor(runtimeapi.SecurityProfile_RuntimeDefault); got != "" {
		t.Errorf("RuntimeDefault → %q, want empty (daemon default)", got)
	}
	if got := seccompProfileFor(runtimeapi.SecurityProfile_Unconfined); got != "unconfined" {
		t.Errorf("Unconfined → %q, want unconfined", got)
	}
	if got := seccompProfileFor(runtimeapi.SecurityProfile_Localhost); got != "" {
		t.Errorf("Localhost → %q, want empty (unsupported)", got)
	}
}

func TestRunRequestForContainer_JoinsSandboxNetns(t *testing.T) {
	c := &criServer{}
	sandbox := &sandboxRecord{
		id:           "pod-1",
		pullrunID:    "wl-pod-1",
		runtimeClass: PullrunContainerRuntimeClass,
	}
	req := &runtimeapi.CreateContainerRequest{
		PodSandboxId: "pod-1",
		Config: &runtimeapi.ContainerConfig{
			Metadata: &runtimeapi.ContainerMetadata{Name: "ctr-1"},
			Image:    &runtimeapi.ImageSpec{Image: "alpine:latest"},
		},
	}
	runReq, err := c.runRequestForContainer("deadbeef"+"deadbeef"+"deadbeef", req, sandbox, "digest")
	if err != nil {
		t.Fatal(err)
	}
	if runReq.NetworkMode != "container:wl-pod-1" {
		t.Errorf("NetworkMode = %q, want container:wl-pod-1", runReq.NetworkMode)
	}
	if runReq.Backend != "container" {
		t.Errorf("Backend = %q, want container", runReq.Backend)
	}
	if !strings.HasPrefix(runReq.Id, "c-") {
		t.Errorf("unexpected workload id %q", runReq.Id)
	}
}

func TestRunRequestForContainer_VMBackendUsesBridge(t *testing.T) {
	c := &criServer{}
	sandbox := &sandboxRecord{
		id:           "pod-vm",
		pullrunID:    "wl-pod-vm",
		runtimeClass: PullrunVMRuntimeClass,
		bridgeName:   "pr-pod-vm",
	}
	req := &runtimeapi.CreateContainerRequest{
		Config: &runtimeapi.ContainerConfig{Metadata: &runtimeapi.ContainerMetadata{Name: "ctr"}},
	}
	runReq, err := c.runRequestForContainer("a1b2c3d4e5f60718293a4b5c", req, sandbox, "digest")
	if err != nil {
		t.Fatal(err)
	}
	if runReq.NetworkMode != "bridge" {
		t.Errorf("NetworkMode = %q, want bridge for VM sandbox", runReq.NetworkMode)
	}
	if runReq.BridgeName != "pr-pod-vm" {
		t.Errorf("BridgeName = %q, want the sandbox's per-pod bridge", runReq.BridgeName)
	}
	if runReq.Backend != "vm" {
		t.Errorf("Backend = %q, want vm", runReq.Backend)
	}
}

func TestRunRequestForContainer_HostNetwork(t *testing.T) {
	c := &criServer{}
	sandbox := &sandboxRecord{
		id:           "pod-h",
		pullrunID:    "wl-pod-h",
		runtimeClass: PullrunContainerRuntimeClass,
		hostNetwork:  true,
	}
	req := &runtimeapi.CreateContainerRequest{
		Config: &runtimeapi.ContainerConfig{Metadata: &runtimeapi.ContainerMetadata{Name: "ctr"}},
	}
	runReq, err := c.runRequestForContainer("aaaaaaaaaaaaaaaaaaaaaaaa", req, sandbox, "digest")
	if err != nil {
		t.Fatal(err)
	}
	if runReq.NetworkMode != "host" {
		t.Errorf("NetworkMode = %q, want host", runReq.NetworkMode)
	}
}

func TestRunRequestForContainer_SecurityContext(t *testing.T) {
	c := &criServer{}
	sandbox := &sandboxRecord{id: "pod-s", pullrunID: "wl-pod-s", runtimeClass: PullrunContainerRuntimeClass}
	req := &runtimeapi.CreateContainerRequest{
		Config: &runtimeapi.ContainerConfig{
			Metadata: &runtimeapi.ContainerMetadata{Name: "ctr-sec"},
			Linux: &runtimeapi.LinuxContainerConfig{
				SecurityContext: &runtimeapi.LinuxContainerSecurityContext{
					Privileged:     true,
					ReadonlyRootfs: true,
					NoNewPrivs:     true,
					Seccomp:        &runtimeapi.SecurityProfile{ProfileType: runtimeapi.SecurityProfile_Unconfined},
				},
			},
		},
	}
	runReq, err := c.runRequestForContainer("bbbbbbbbbbbbbbbbbbbbbbbb", req, sandbox, "digest")
	if err != nil {
		t.Fatal(err)
	}
	if !runReq.Privileged {
		t.Error("Privileged = false, want true")
	}
	if !runReq.ReadonlyRootfs {
		t.Error("ReadonlyRootfs = false, want true")
	}
	if !runReq.NoNewPrivileges {
		t.Error("NoNewPrivileges = false, want true")
	}
	if runReq.SeccompProfile != "unconfined" {
		t.Errorf("SeccompProfile = %q, want unconfined", runReq.SeccompProfile)
	}
}

func TestRunRequestForContainer_CommandEnvResources(t *testing.T) {
	c := &criServer{}
	sandbox := &sandboxRecord{id: "pod-e", pullrunID: "wl-pod-e", runtimeClass: PullrunContainerRuntimeClass}
	req := &runtimeapi.CreateContainerRequest{
		Config: &runtimeapi.ContainerConfig{
			Metadata: &runtimeapi.ContainerMetadata{Name: "ctr-env"},
			Command:  []string{"/bin/app"},
			Args:     []string{"--serve", "--port=8080"},
			Envs: []*runtimeapi.KeyValue{
				{Key: "A", Value: "1"},
				{Key: "B", Value: "2"},
			},
			WorkingDir: "/app",
			Linux: &runtimeapi.LinuxContainerConfig{
				Resources: &runtimeapi.LinuxContainerResources{
					CpuPeriod:          100000,
					CpuQuota:           200000,
					MemoryLimitInBytes: 512 * 1024 * 1024,
				},
			},
		},
	}
	runReq, err := c.runRequestForContainer("cccccccccccccccccccccccc", req, sandbox, "digest")
	if err != nil {
		t.Fatal(err)
	}
	if len(runReq.Command) != 3 || runReq.Command[0] != "/bin/app" || runReq.Command[2] != "--port=8080" {
		t.Errorf("Command = %v, want [/bin/app --serve --port=8080]", runReq.Command)
	}
	if runReq.Env["A"] != "1" || runReq.Env["B"] != "2" {
		t.Errorf("Env = %v, want A=1 B=2", runReq.Env)
	}
	if runReq.CpuMillicores != 2000 {
		t.Errorf("CpuMillicores = %d, want 2000", runReq.CpuMillicores)
	}
	if runReq.MemoryBytes != 512*1024*1024 {
		t.Errorf("MemoryBytes = %d, want %d", runReq.MemoryBytes, 512*1024*1024)
	}
	if runReq.WorkingDir != "/app" {
		t.Errorf("WorkingDir = %q, want /app", runReq.WorkingDir)
	}
}
