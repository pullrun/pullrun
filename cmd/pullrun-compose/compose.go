// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/compose-spec/compose-go/v2/loader"
	"github.com/compose-spec/compose-go/v2/types"
	"gopkg.in/yaml.v3"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

// toProtoService converts a single compose service to the proto type.
func toProtoService(name string, svc types.ServiceConfig, backend string) *runtimeapi.ComposeService {
	env := make(map[string]string)
	for k, v := range svc.Environment {
		if v != nil {
			env[k] = *v
		}
	}

	var ports []*runtimeapi.ComposePort
	for _, p := range svc.Ports {
		proto := "tcp"
		if p.Protocol == "udp" {
			proto = "udp"
		}
		ports = append(ports, &runtimeapi.ComposePort{
			ContainerPort: p.Target,
			HostPort:      parsePublishedPort(p.Published),
			Protocol:      proto,
		})
	}

	var dependsOn []string
	for dep := range svc.DependsOn {
		dependsOn = append(dependsOn, dep)
	}

	labels := make(map[string]string)
	for k, v := range svc.Labels {
		labels[k] = v
	}

	cpuMillis := uint64(0)
	memBytes := uint64(0)
	if svc.Deploy != nil {
		if svc.Deploy.Resources.Limits != nil {
			if svc.Deploy.Resources.Limits.NanoCPUs > 0 {
				cpuMillis = uint64(svc.Deploy.Resources.Limits.NanoCPUs / 1_000_000)
			}
			if svc.Deploy.Resources.Limits.MemoryBytes > 0 {
				memBytes = uint64(svc.Deploy.Resources.Limits.MemoryBytes)
			}
		}
	}

	workingDir := svc.WorkingDir
	if workingDir == "" {
		workingDir = "/"
	}

	// Convert volumes
	var mounts []*runtimeapi.Mount
	for _, v := range svc.Volumes {
		var opts []string
		if v.Type == "bind" {
			opts = append(opts, "rbind", "rprivate")
			if v.ReadOnly {
				opts = append(opts, "ro")
			}
		}
		mounts = append(mounts, &runtimeapi.Mount{
			Type:        v.Type,
			Source:      v.Source,
			Destination: v.Target,
			Options:     opts,
		})
	}

	// Default network mode: slirp (userspace NAT) for VMs,
	// bridge (kernel TAP+bridge+iptables) for containers.
	netMode := "bridge"
	if backend == "vm" {
		netMode = "slirp"
	}

	return &runtimeapi.ComposeService{
		Name:          name,
		Image:         svc.Image,
		Command:       svc.Command,
		Environment:   env,
		Ports:         ports,
		WorkingDir:    workingDir,
		Labels:        labels,
		NetworkMode:   netMode,
		CpuMillicores: cpuMillis,
		MemoryBytes:   memBytes,
		DependsOn:     dependsOn,
		Mounts:        mounts,
	}
}

func parsePublishedPort(s string) uint32 {
	if s == "" {
		return 0
	}
	var port uint32
	_, _ = fmt.Sscanf(s, "%d", &port)
	return port
}

// sortServices orders service names so that dependencies come first.
func sortServices(names []string, services map[string]types.ServiceConfig) {
	target := services
	visited := make(map[string]bool)
	inStack := make(map[string]bool)
	var order []string

	var visit func(name string)
	visit = func(name string) {
		if visited[name] {
			return
		}
		if inStack[name] {
			return
		}
		inStack[name] = true
		svc := target[name]
		for dep := range svc.DependsOn {
			if _, ok := target[dep]; ok {
				visit(dep)
			}
		}
		inStack[name] = false
		visited[name] = true
		order = append(order, name)
	}

	for _, name := range names {
		if !visited[name] {
			visit(name)
		}
	}

	copy(names, order)
}

// parseComposeYAML parses a docker-compose YAML file and returns a Project.
func parseComposeYAML(path string) (*types.Project, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}

	abs, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	projectName := filepath.Base(filepath.Dir(abs))

	var raw struct {
		Name string `yaml:"name"`
	}
	if err := yaml.Unmarshal(data, &raw); err == nil && raw.Name != "" {
		projectName = raw.Name
	}

	workingDir := filepath.Dir(abs)

	project, err := loader.LoadWithContext(context.Background(), types.ConfigDetails{
		WorkingDir: workingDir,
		ConfigFiles: []types.ConfigFile{
			{
				Filename: path,
				Content:  data,
			},
		},
	}, func(opts *loader.Options) {
		opts.SetProjectName(projectName, true)
	})
	if err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}

	return project, nil
}
