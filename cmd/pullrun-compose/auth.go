package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

const authFileName = "auth.json"

// RegistryAuth stores credentials for a single registry.
type RegistryAuth struct {
	Username string `json:"username,omitempty"`
	Password string `json:"password,omitempty"`
	Token    string `json:"token,omitempty"`
}

// AuthConfig is the on-disk format for stored registry credentials.
type AuthConfig struct {
	Registries map[string]RegistryAuth `json:"registries"`
}

var (
	authMu    sync.Mutex
	authCache *AuthConfig
)

// authPath returns the path to the auth config file.
func authPath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("home dir: %w", err)
	}
	dir := filepath.Join(home, ".pullrun")
	if err := os.MkdirAll(dir, 0700); err != nil {
		return "", fmt.Errorf("mkdir %s: %w", dir, err)
	}
	return filepath.Join(dir, authFileName), nil
}

// loadAuth reads the auth config file.
func loadAuth() (*AuthConfig, error) {
	authMu.Lock()
	defer authMu.Unlock()

	if authCache != nil {
		return authCache, nil
	}

	path, err := authPath()
	if err != nil {
		return nil, err
	}

	cfg := &AuthConfig{Registries: make(map[string]RegistryAuth)}
	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			authCache = cfg
			return cfg, nil
		}
		return nil, fmt.Errorf("read %s: %w", path, err)
	}

	if err := json.Unmarshal(data, cfg); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	if cfg.Registries == nil {
		cfg.Registries = make(map[string]RegistryAuth)
	}
	authCache = cfg
	return cfg, nil
}

// GetRegistryAuth returns stored credentials for a given registry host.
func GetRegistryAuth(registry string) (*RegistryAuth, error) {
	cfg, err := loadAuth()
	if err != nil {
		return nil, err
	}
	a, ok := cfg.Registries[registry]
	if !ok {
		return nil, nil
	}
	return &a, nil
}

// NormalizeRegistry normalizes registry names.
func NormalizeRegistry(registry string) string {
	if registry == "" || registry == "docker.io" || registry == "registry-1.docker.io" {
		return "docker.io"
	}
	return registry
}

// extractRegistryFromRef extracts the registry host from an image reference.
func extractRegistryFromRef(ref string) string {
	host, _, _ := splitImageRef(ref)
	if host == "" {
		return "docker.io"
	}
	return host
}

// splitImageRef splits "registry.example.com/namespace/repo:tag".
func splitImageRef(ref string) (string, string, string) {
	if ref == "" {
		return "", "", ""
	}

	tag := "latest"
	idx := strings.LastIndex(ref, ":")
	if idx > 0 && idx < len(ref)-1 && ref[idx-1] != '/' {
		tag = ref[idx+1:]
		ref = ref[:idx]
	}

	idx = strings.Index(ref, "/")
	if idx < 0 {
		return "", ref, tag
	}

	host := ref[:idx]
	path := ref[idx+1:]

	if strings.Contains(host, ".") || strings.Contains(host, ":") || host == "localhost" || host == "docker.io" || host == "registry-1.docker.io" {
		return host, path, tag
	}

	return "", ref, tag
}
