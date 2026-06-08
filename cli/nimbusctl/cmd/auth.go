package cmd

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

// authPath returns the path to the auth config file.
func authPath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("home dir: %w", err)
	}
	dir := filepath.Join(home, ".nimbus")
	if err := os.MkdirAll(dir, 0700); err != nil {
		return "", fmt.Errorf("mkdir %s: %w", dir, err)
	}
	return filepath.Join(dir, authFileName), nil
}

var (
	authMu    sync.Mutex
	authCache *AuthConfig
)

// loadAuth reads the auth config file. Returns an empty config if the file
// doesn't exist. Results are cached in memory.
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

// saveAuth writes the auth config to disk, with 0600 perms.
func saveAuth(cfg *AuthConfig) error {
	authMu.Lock()
	defer authMu.Unlock()

	path, err := authPath()
	if err != nil {
		return err
	}

	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal auth: %w", err)
	}

	if err := os.WriteFile(path, data, 0600); err != nil {
		return fmt.Errorf("write %s: %w", path, err)
	}
	authCache = cfg
	return nil
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

// SetRegistryAuth stores credentials for a given registry host.
func SetRegistryAuth(registry string, auth RegistryAuth) error {
	cfg, err := loadAuth()
	if err != nil {
		return err
	}
	cfg.Registries[registry] = auth
	return saveAuth(cfg)
}

// RemoveRegistryAuth deletes stored credentials for a given registry host.
func RemoveRegistryAuth(registry string) error {
	cfg, err := loadAuth()
	if err != nil {
		return err
	}
	delete(cfg.Registries, registry)
	return saveAuth(cfg)
}

// ListRegistries returns all registry hosts with stored credentials.
func ListRegistries() ([]string, error) {
	cfg, err := loadAuth()
	if err != nil {
		return nil, err
	}
	keys := make([]string, 0, len(cfg.Registries))
	for k := range cfg.Registries {
		keys = append(keys, k)
	}
	return keys, nil
}

// NormalizeRegistry normalizes registry names: "docker.io" and empty
// string are both treated as "docker.io" for credential lookup.
func NormalizeRegistry(registry string) string {
	if registry == "" || registry == "docker.io" || registry == "registry-1.docker.io" {
		return "docker.io"
	}
	return registry
}

// extractRegistryFromRef extracts the registry host from an image reference
// like "registry.example.com/myapp:latest" or "docker.io/library/alpine:latest".
// Returns "docker.io" if no explicit registry is found.
func extractRegistryFromRef(ref string) string {
	host, _, _ := splitImageRef(ref)
	if host == "" {
		return "docker.io"
	}
	return host
}

// splitImageRef splits "registry.example.com/namespace/repo:tag" into
// (host, path, tag). Returns ("", "", "") on failure.
func splitImageRef(ref string) (string, string, string) {
	if ref == "" {
		return "", "", ""
	}

	// Split tag
	tag := "latest"
	idx := strings.LastIndex(ref, ":")
	if idx > 0 && idx < len(ref)-1 && ref[idx-1] != '/' {
		tag = ref[idx+1:]
		ref = ref[:idx]
	}

	// Split path from host
	idx = strings.Index(ref, "/")
	if idx < 0 {
		return "", ref, tag
	}

	host := ref[:idx]
	path := ref[idx+1:]

	// Check if it looks like a host:port or hostname
	if strings.Contains(host, ".") || strings.Contains(host, ":") || host == "localhost" || host == "docker.io" || host == "registry-1.docker.io" {
		return host, path, tag
	}

	// No registry host, it's like "library/alpine:latest"
	return "", ref, tag
}
