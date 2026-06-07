package registry

import (
	"crypto/sha256"
	"encoding/hex"
	"io"
	"sync"
)

// Phase 3 placeholder for a pull-through registry cache.
// Nimbus DAG nodes are content-addressed by sha256, so this can
// simply forward unknown requests to upstream and cache the result.

type CacheEntry struct {
	Digest string
	Size   int64
	Data   []byte
}

type PullThroughCache struct {
	mu      sync.RWMutex
	entries map[string]*CacheEntry
	upstream string
}

func NewPullThroughCache(upstream string) *PullThroughCache {
	return &PullThroughCache{
		entries:  make(map[string]*CacheEntry),
		upstream: upstream,
	}
}

func (c *PullThroughCache) Has(digest string) bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	_, ok := c.entries[digest]
	return ok
}

func (c *PullThroughCache) Put(digest string, data []byte) error {
	actual := sha256.Sum256(data)
	computed := hex.EncodeToString(actual[:])
	if computed != digest {
		return ErrDigestMismatch
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entries[digest] = &CacheEntry{
		Digest: digest,
		Size:   int64(len(data)),
		Data:   data,
	}
	return nil
}

func (c *PullThroughCache) Get(digest string) (*CacheEntry, error) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	e, ok := c.entries[digest]
	if !ok {
		return nil, ErrNotFound
	}
	return e, nil
}

func (c *PullThroughCache) Count() int {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return len(c.entries)
}

var (
	ErrNotFound       = &RegistryError{Message: "digest not in cache"}
	ErrDigestMismatch = &RegistryError{Message: "computed digest does not match expected"}
)

type RegistryError struct {
	Message string
}

func (e *RegistryError) Error() string {
	return e.Message
}

var _ = io.EOF