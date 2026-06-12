// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
)

func TestNewFileStore_CreatesDirs(t *testing.T) {
	dir := t.TempDir()
	s := newFileStore(dir)
	if s == nil {
		t.Fatal("newFileStore returned nil")
	}
	if _, err := os.Stat(filepath.Join(dir, "sandboxes")); err != nil {
		t.Errorf("sandboxes dir not created: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, "containers")); err != nil {
		t.Errorf("containers dir not created: %v", err)
	}
}

func TestPutGetSandbox_Roundtrip(t *testing.T) {
	s := newFileStore(t.TempDir())
	rec := &sandboxRecord{
		id:        "sb-1",
		pullrunID:  "wl-1",
		namespace: "default",
		name:      "test-sb",
		createdAt: time.Unix(1000, 0),
		state:     runtimeapi.PodSandboxState_SANDBOX_READY,
		internalIP: "10.0.0.2",
	}
	s.putSandbox(rec)

	got, ok := s.getSandbox("sb-1")
	if !ok {
		t.Fatal("getSandbox returned not found")
	}
	if got.pullrunID != "wl-1" {
		t.Errorf("pullrunID = %q, want wl-1", got.pullrunID)
	}
	if got.state != runtimeapi.PodSandboxState_SANDBOX_READY {
		t.Errorf("state = %v, want SANDBOX_READY", got.state)
	}
}

func TestRemoveSandbox_RemovesFromMap(t *testing.T) {
	s := newFileStore(t.TempDir())
	s.putSandbox(&sandboxRecord{id: "sb-1", createdAt: time.Unix(1, 0)})
	s.removeSandbox("sb-1")
	_, ok := s.getSandbox("sb-1")
	if ok {
		t.Error("getSandbox returned ok after remove")
	}
}

func TestRemoveSandbox_RemovesFile(t *testing.T) {
	dir := t.TempDir()
	s := newFileStore(dir)
	s.putSandbox(&sandboxRecord{id: "sb-1", createdAt: time.Unix(1, 0)})
	s.removeSandbox("sb-1")

	s2 := newFileStore(dir)
	_, ok := s2.getSandbox("sb-1")
	if ok {
		t.Error("sandbox persisted after remove (reload found it)")
	}
}

func TestPutGetContainer_Roundtrip(t *testing.T) {
	s := newFileStore(t.TempDir())
	rec := &containerRecord{
		id:       "c-1",
		sandboxID: "sb-1",
		pullrunID: "wl-2",
		name:     "test-ctr",
		image:    "alpine:latest",
		createdAt: time.Unix(2000, 0),
	}
	s.putContainer(rec)

	got, ok := s.getContainer("c-1")
	if !ok {
		t.Fatal("getContainer returned not found")
	}
	if got.sandboxID != "sb-1" {
		t.Errorf("sandboxID = %q, want sb-1", got.sandboxID)
	}
	if got.image != "alpine:latest" {
		t.Errorf("image = %q, want alpine:latest", got.image)
	}
}

func TestRemoveContainer(t *testing.T) {
	s := newFileStore(t.TempDir())
	s.putContainer(&containerRecord{id: "c-1", createdAt: time.Unix(1, 0)})
	s.removeContainer("c-1")
	_, ok := s.getContainer("c-1")
	if ok {
		t.Error("getContainer returned ok after remove")
	}
}

func TestRemoveSandbox_CascadesContainers(t *testing.T) {
	s := newFileStore(t.TempDir())
	s.putSandbox(&sandboxRecord{id: "sb-1", createdAt: time.Unix(1, 0)})
	s.putContainer(&containerRecord{id: "c-1", sandboxID: "sb-1", createdAt: time.Unix(1, 0)})
	s.putContainer(&containerRecord{id: "c-2", sandboxID: "sb-1", createdAt: time.Unix(2, 0)})
	s.removeSandbox("sb-1")

	if _, ok := s.getContainer("c-1"); ok {
		t.Error("container c-1 still exists after sandbox removal")
	}
	if _, ok := s.getContainer("c-2"); ok {
		t.Error("container c-2 still exists after sandbox removal")
	}
}

func TestPersistence_AcrossReload(t *testing.T) {
	dir := t.TempDir()
	s1 := newFileStore(dir)
	s1.putSandbox(&sandboxRecord{id: "sb-persist", pullrunID: "wl-p", createdAt: time.Unix(10, 0), state: runtimeapi.PodSandboxState_SANDBOX_NOTREADY})

	s2 := newFileStore(dir)
	got, ok := s2.getSandbox("sb-persist")
	if !ok {
		t.Fatal("sandbox not found after reload")
	}
	if got.pullrunID != "wl-p" {
		t.Errorf("pullrunID = %q after reload, want wl-p", got.pullrunID)
	}
	if got.state != runtimeapi.PodSandboxState_SANDBOX_NOTREADY {
		t.Errorf("state = %v after reload, want SANDBOX_NOTREADY", got.state)
	}
}

func TestListSandboxes_WithFilter(t *testing.T) {
	s := newFileStore(t.TempDir())
	s.putSandbox(&sandboxRecord{id: "sb-a", state: runtimeapi.PodSandboxState_SANDBOX_READY, createdAt: time.Unix(1, 0)})
	s.putSandbox(&sandboxRecord{id: "sb-b", state: runtimeapi.PodSandboxState_SANDBOX_NOTREADY, createdAt: time.Unix(2, 0)})
	s.putSandbox(&sandboxRecord{id: "sb-c", state: runtimeapi.PodSandboxState_SANDBOX_READY, createdAt: time.Unix(3, 0)})

	all := s.listSandboxes(nil)
	if len(all) != 3 {
		t.Errorf("listSandboxes(nil) = %d, want 3", len(all))
	}

	filter := &runtimeapi.PodSandboxFilter{
		Id: "sb-b",
	}
	got := s.listSandboxes(filter)
	if len(got) != 1 {
		t.Fatalf("listSandboxes with id filter = %d, want 1", len(got))
	}
}

func TestListContainers_WithFilter(t *testing.T) {
	s := newFileStore(t.TempDir())
	s.putContainer(&containerRecord{id: "c-a", sandboxID: "sb-1", createdAt: time.Unix(1, 0)})
	s.putContainer(&containerRecord{id: "c-b", sandboxID: "sb-1", createdAt: time.Unix(2, 0)})
	s.putContainer(&containerRecord{id: "c-c", sandboxID: "sb-2", createdAt: time.Unix(3, 0)})

	all := s.listContainers(nil)
	if len(all) != 3 {
		t.Errorf("listContainers(nil) = %d, want 3", len(all))
	}

	filter := &runtimeapi.ContainerFilter{
		PodSandboxId: "sb-1",
	}
	got := s.listContainers(filter)
	if len(got) != 2 {
		t.Fatalf("listContainers with sandbox filter = %d, want 2", len(got))
	}
}
