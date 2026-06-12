// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/compose-spec/compose-go/v2/types"
)

func TestSortServices_Linear(t *testing.T) {
	svcs := map[string]types.ServiceConfig{
		"db": {
			DependsOn: types.DependsOnConfig{},
		},
		"app": {
			DependsOn: types.DependsOnConfig{
				"db": types.ServiceDependency{Condition: "service_started"},
			},
		},
		"web": {
			DependsOn: types.DependsOnConfig{
				"app": types.ServiceDependency{Condition: "service_started"},
			},
		},
	}
	names := []string{"web", "db", "app"}
	sortServices(names, svcs)

	assertBefore(t, names, "db", "app")
	assertBefore(t, names, "app", "web")
}

func TestSortServices_NoDeps(t *testing.T) {
	svcs := map[string]types.ServiceConfig{
		"a": {},
		"b": {},
		"c": {},
	}
	names := []string{"c", "a", "b"}
	sortServices(names, svcs)
	if len(names) != 3 {
		t.Fatalf("len=%d, want 3", len(names))
	}
}

func TestSortServices_DiamondDependency(t *testing.T) {
	svcs := map[string]types.ServiceConfig{
		"base": {},
		"left": {
			DependsOn: types.DependsOnConfig{
				"base": types.ServiceDependency{Condition: "service_started"},
			},
		},
		"right": {
			DependsOn: types.DependsOnConfig{
				"base": types.ServiceDependency{Condition: "service_started"},
			},
		},
		"top": {
			DependsOn: types.DependsOnConfig{
				"left":  types.ServiceDependency{Condition: "service_started"},
				"right": types.ServiceDependency{Condition: "service_started"},
			},
		},
	}
	names := []string{"top", "left", "right", "base"}
	sortServices(names, svcs)
	assertBefore(t, names, "base", "left")
	assertBefore(t, names, "base", "right")
	assertBefore(t, names, "left", "top")
	assertBefore(t, names, "right", "top")
}

func TestSortServices_CycleDoesNotInfiniteLoop(t *testing.T) {
	svcs := map[string]types.ServiceConfig{
		"a": {
			DependsOn: types.DependsOnConfig{
				"b": types.ServiceDependency{Condition: "service_started"},
			},
		},
		"b": {
			DependsOn: types.DependsOnConfig{
				"a": types.ServiceDependency{Condition: "service_started"},
			},
		},
	}
	names := []string{"a", "b"}
	sortServices(names, svcs)
	// Must not infinite-loop; any order is acceptable for a cycle.
	if len(names) != 2 {
		t.Fatalf("len=%d, want 2", len(names))
	}
}

func TestToProtoService_PortMapping(t *testing.T) {
	svc := types.ServiceConfig{
		Image: "nginx:latest",
		Ports: []types.ServicePortConfig{
			{Target: 80, Published: "8080", Protocol: "tcp"},
			{Target: 53, Published: "5353", Protocol: "udp"},
		},
	}
	proto := toProtoService("web", svc)
	if proto.Image != "nginx:latest" {
		t.Errorf("Image = %q, want nginx:latest", proto.Image)
	}
	if len(proto.Ports) != 2 {
		t.Fatalf("len(ports) = %d, want 2", len(proto.Ports))
	}
	if proto.Ports[0].ContainerPort != 80 || proto.Ports[0].HostPort != 8080 || proto.Ports[0].Protocol != "tcp" {
		t.Errorf("port 0 = %+v", proto.Ports[0])
	}
	if proto.Ports[1].Protocol != "udp" {
		t.Errorf("port 1 protocol = %q, want udp", proto.Ports[1].Protocol)
	}
}

func TestToProtoService_Environment(t *testing.T) {
	svc := types.ServiceConfig{
		Image: "alpine:latest",
		Environment: map[string]*string{
			"FOO": strPtr("bar"),
			"BAZ": strPtr("qux"),
		},
	}
	proto := toProtoService("env-test", svc)
	if len(proto.Environment) != 2 {
		t.Fatalf("len(env) = %d, want 2", len(proto.Environment))
	}
	if proto.Environment["FOO"] != "bar" {
		t.Errorf("FOO = %q, want bar", proto.Environment["FOO"])
	}
}

func TestToProtoService_DeployResources(t *testing.T) {
	svc := types.ServiceConfig{
		Image: "redis:latest",
		Deploy: &types.DeployConfig{
			Resources: types.Resources{
				Limits: &types.Resource{
					NanoCPUs:    types.NanoCPUs(500000000),
					MemoryBytes: 268435456,
				},
			},
		},
	}
	proto := toProtoService("cache", svc)
	if proto.CpuMillicores != 500 {
		t.Errorf("CpuMillicores = %d, want 500", proto.CpuMillicores)
	}
	if proto.MemoryBytes != 268435456 {
		t.Errorf("MemoryBytes = %d, want 268435456", proto.MemoryBytes)
	}
}

func TestToProtoService_WorkingDirDefault(t *testing.T) {
	svc := types.ServiceConfig{
		Image: "busybox:latest",
	}
	proto := toProtoService("worker", svc)
	if proto.WorkingDir != "/" {
		t.Errorf("WorkingDir = %q, want /", proto.WorkingDir)
	}
}

func TestParsePublishedPort_Valid(t *testing.T) {
	if got := parsePublishedPort("8080"); got != 8080 {
		t.Errorf("parsePublishedPort(8080) = %d", got)
	}
}

func TestParsePublishedPort_Empty(t *testing.T) {
	if got := parsePublishedPort(""); got != 0 {
		t.Errorf("parsePublishedPort('') = %d", got)
	}
}

func TestParsePublishedPort_RangeSyntax(t *testing.T) {
	if got := parsePublishedPort("3000-3005"); got != 3000 {
		t.Errorf("parsePublishedPort('3000-3005') = %d, want 3000", got)
	}
}

func TestParseComposeYAML_Valid(t *testing.T) {
	dir := t.TempDir()
	yml := `name: testproj
services:
  app:
    image: nginx:latest
    ports:
      - "8080:80"
`
	path := filepath.Join(dir, "docker-compose.yml")
	if err := os.WriteFile(path, []byte(yml), 0644); err != nil {
		t.Fatal(err)
	}

	proj, err := parseComposeYAML(path)
	if err != nil {
		t.Fatalf("parseComposeYAML: %v", err)
	}
	if proj.Name != "testproj" {
		t.Errorf("project name = %q, want testproj", proj.Name)
	}
	if _, ok := proj.Services["app"]; !ok {
		t.Error("service 'app' not found")
	}
}

func TestParseComposeYAML_InfersNameFromDir(t *testing.T) {
	dir := t.TempDir()
	yml := `services:
  web:
    image: nginx:latest
`
	path := filepath.Join(dir, "docker-compose.yml")
	if err := os.WriteFile(path, []byte(yml), 0644); err != nil {
		t.Fatal(err)
	}

	proj, err := parseComposeYAML(path)
	if err != nil {
		t.Fatalf("parseComposeYAML: %v", err)
	}
	if proj.Name == "" {
		t.Error("project name should be inferred from dir, got empty")
	}
}

func TestParseComposeYAML_InvalidYAML(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "docker-compose.yml")
	if err := os.WriteFile(path, []byte(": broken yaml"), 0644); err != nil {
		t.Fatal(err)
	}

	_, err := parseComposeYAML(path)
	if err == nil {
		t.Error("expected error for invalid YAML")
	}
}

func TestParseComposeYAML_FileNotFound(t *testing.T) {
	_, err := parseComposeYAML("/nonexistent/path/docker-compose.yml")
	if err == nil {
		t.Error("expected error for missing file")
	}
}

func assertBefore(t *testing.T, names []string, a, b string) {
	t.Helper()
	pos := make(map[string]int)
	for i, n := range names {
		pos[n] = i
	}
	if pos[a] > pos[b] {
		t.Errorf("%q should appear before %q, got %v", a, b, names)
	}
}

func strPtr(s string) *string { return &s }
