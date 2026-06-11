package main

import (
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"sync"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
)

// fileStore is a persistent sandbox/container store backed by JSON files
// on disk. It survives CRI shim restarts.
type fileStore struct {
	mu         sync.RWMutex
	root       string
	sandboxes  map[string]*sandboxRecord
	containers map[string]*containerRecord
}

// sandboxCheckpoint is the disk-serializable form of sandboxRecord.
type sandboxCheckpoint struct {
	ID           string    `json:"id"`
	PullrunID     string    `json:"pullrun_id"`
	Namespace    string    `json:"namespace"`
	Name         string    `json:"name"`
	CreatedAt    time.Time `json:"created_at"`
	State        int32     `json:"state"`
	InternalIP   string    `json:"internal_ip"`
	RuntimeClass string    `json:"runtime_class"`
}

// containerCheckpoint is the disk-serializable form of containerRecord.
type containerCheckpoint struct {
	ID        string    `json:"id"`
	SandboxID string    `json:"sandbox_id"`
	PullrunID  string    `json:"pullrun_id"`
	Name      string    `json:"name"`
	Image     string    `json:"image"`
	CreatedAt time.Time `json:"created_at"`
}

func newFileStore(root string) *fileStore {
	s := &fileStore{
		root:       root,
		sandboxes:  make(map[string]*sandboxRecord),
		containers: make(map[string]*containerRecord),
	}
	for _, d := range []string{"sandboxes", "containers"} {
		if err := os.MkdirAll(filepath.Join(root, d), 0o755); err != nil {
			log.Printf("failed to create %s store directory: %v", d, err)
		}
	}
	s.loadAll()
	return s
}

func (s *fileStore) loadAll() {
	sandboxDir := filepath.Join(s.root, "sandboxes")
	if entries, err := os.ReadDir(sandboxDir); err == nil {
		for _, e := range entries {
			if e.IsDir() || filepath.Ext(e.Name()) != ".json" {
				continue
			}
			path := filepath.Join(sandboxDir, e.Name())
			data, err := os.ReadFile(path)
			if err != nil {
				log.Printf("warning: cannot read sandbox checkpoint %s: %v", path, err)
				continue
			}
			var cp sandboxCheckpoint
			if err := json.Unmarshal(data, &cp); err != nil {
				log.Printf("warning: invalid sandbox checkpoint %s: %v", path, err)
				continue
			}
			s.sandboxes[cp.ID] = &sandboxRecord{
				id:           cp.ID,
				pullrunID:     cp.PullrunID,
				namespace:    cp.Namespace,
				name:         cp.Name,
				createdAt:    cp.CreatedAt,
				state:        runtimeapi.PodSandboxState(cp.State),
				internalIP:   cp.InternalIP,
				runtimeClass: cp.RuntimeClass,
			}
		}
	}
	log.Printf("recovered %d sandboxes from %s", len(s.sandboxes), sandboxDir)

	containerDir := filepath.Join(s.root, "containers")
	if entries, err := os.ReadDir(containerDir); err == nil {
		for _, e := range entries {
			if e.IsDir() || filepath.Ext(e.Name()) != ".json" {
				continue
			}
			path := filepath.Join(containerDir, e.Name())
			data, err := os.ReadFile(path)
			if err != nil {
				log.Printf("warning: cannot read container checkpoint %s: %v", path, err)
				continue
			}
			var cp containerCheckpoint
			if err := json.Unmarshal(data, &cp); err != nil {
				log.Printf("warning: invalid container checkpoint %s: %v", path, err)
				continue
			}
			s.containers[cp.ID] = &containerRecord{
				id:        cp.ID,
				sandboxID: cp.SandboxID,
				pullrunID:  cp.PullrunID,
				name:      cp.Name,
				image:     cp.Image,
				createdAt: cp.CreatedAt,
			}
		}
	}
	log.Printf("recovered %d containers from %s", len(s.containers), containerDir)
}

func (s *fileStore) putSandbox(rec *sandboxRecord) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.sandboxes[rec.id] = rec
	s.writeSandbox(rec)
}

func (s *fileStore) getSandbox(id string) (*sandboxRecord, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rec, ok := s.sandboxes[id]
	return rec, ok
}

func (s *fileStore) listSandboxes(filter *runtimeapi.PodSandboxFilter) []*runtimeapi.PodSandbox {
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

func (s *fileStore) removeSandbox(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.sandboxes, id)
	os.Remove(filepath.Join(s.root, "sandboxes", id+".json"))
	for cid, c := range s.containers {
		if c.sandboxID == id {
			delete(s.containers, cid)
			os.Remove(filepath.Join(s.root, "containers", cid+".json"))
		}
	}
}

func (s *fileStore) putContainer(rec *containerRecord) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.containers[rec.id] = rec
	s.writeContainer(rec)
}

func (s *fileStore) getContainer(id string) (*containerRecord, bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rec, ok := s.containers[id]
	return rec, ok
}

func (s *fileStore) removeContainer(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.containers, id)
	os.Remove(filepath.Join(s.root, "containers", id+".json"))
}

func (s *fileStore) listContainers(filter *runtimeapi.ContainerFilter) []*runtimeapi.Container {
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

func (s *fileStore) writeSandbox(rec *sandboxRecord) {
	cp := sandboxCheckpoint{
		ID:           rec.id,
		PullrunID:     rec.pullrunID,
		Namespace:    rec.namespace,
		Name:         rec.name,
		CreatedAt:    rec.createdAt,
		State:        int32(rec.state),
		InternalIP:   rec.internalIP,
		RuntimeClass: rec.runtimeClass,
	}
	data, err := json.Marshal(cp)
	if err != nil {
		log.Printf("error marshaling sandbox %s: %v", rec.id, err)
		return
	}
	path := filepath.Join(s.root, "sandboxes", rec.id+".json")
	if err := os.WriteFile(path, data, 0o644); err != nil {
		log.Printf("error writing sandbox checkpoint %s: %v", path, err)
	}
}

func (s *fileStore) writeContainer(rec *containerRecord) {
	cp := containerCheckpoint{
		ID:        rec.id,
		SandboxID: rec.sandboxID,
		PullrunID:  rec.pullrunID,
		Name:      rec.name,
		Image:     rec.image,
		CreatedAt: rec.createdAt,
	}
	data, err := json.Marshal(cp)
	if err != nil {
		log.Printf("error marshaling container %s: %v", rec.id, err)
		return
	}
	path := filepath.Join(s.root, "containers", rec.id+".json")
	if err := os.WriteFile(path, data, 0o644); err != nil {
		log.Printf("error writing container checkpoint %s: %v", path, err)
	}
}
