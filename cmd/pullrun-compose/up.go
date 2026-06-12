// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

// bridgeNameForProject derives a deterministic bridge name from the project name.
func bridgeNameForProject(projectName string) string {
	h := sha256.Sum256([]byte(projectName))
	return fmt.Sprintf("pullrun-%x", h[:4])
}

func newUpCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "up [service...]",
		Short: "Create and start services",
		Long:  `Create and start all services defined in the compose file. If service names are given as arguments, only those services are started.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			filePath := resolveComposeFile()
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				return fmt.Errorf("compose file not found: %s (use -f to specify)", filePath)
			}

			fmt.Printf("Reading %s...\n", filePath)
			project, err := parseComposeYAML(filePath)
			if err != nil {
				return fmt.Errorf("parse compose: %w", err)
			}

			fmt.Printf("Project: %s (%d services)\n", project.Name, len(project.Services))

			// Derive per-project bridge for network isolation (D4).
			bridgeName := bridgeNameForProject(project.Name)
			if verbose {
				fmt.Fprintf(os.Stderr, "  bridge: %s (per-project isolation)\n", bridgeName)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
			defer cancel()

			client, conn, err := connectRuntime(ctx)
			if err != nil {
				return fmt.Errorf("connect to pullrun runtime: %w", err)
			}
			defer conn.Close()

			names := project.ServiceNames()
			if len(args) > 0 {
				names = args
			}
			sortServices(names, project.Services)

			for _, name := range names {
				svc, ok := project.Services[name]
				if !ok {
					return fmt.Errorf("service %q not found in compose file", name)
				}

				id := fmt.Sprintf("%s-%s", project.Name, name)
				fmt.Printf("  %s (%s)... ", name, svc.Image)

				registry := extractRegistryFromRef(svc.Image)
			auth, _ := GetRegistryAuth(NormalizeRegistry(registry))
			pullReq := &runtimeapi.PullImageRequest{
				ImageRef: svc.Image,
				Registry: registry,
			}
			if auth != nil {
				pullReq.RegistryUsername = auth.Username
				pullReq.RegistryPassword = auth.Password
				pullReq.RegistryToken = auth.Token
			}
			pullResp, err := client.PullImage(ctx, pullReq)
				if err != nil {
					fmt.Fprintf(os.Stderr, "\n  %s: PULL FAILED: %v\n", name, err)
					return fmt.Errorf("pull %s: %w", svc.Image, err)
				}

				var networkRules []*runtimeapi.NetworkRule
				for _, p := range svc.Ports {
					proto := "tcp"
					if p.Protocol == "udp" {
						proto = "udp"
					}
					networkRules = append(networkRules, &runtimeapi.NetworkRule{
						Direction: "inbound",
						Protocol:  proto,
						Port:      uint32(p.Target),
					})
				}

				env := make(map[string]string)
				for k, v := range svc.Environment {
					if v != nil {
						env[k] = *v
					}
				}

				workingDir := svc.WorkingDir
				if workingDir == "" {
					workingDir = "/"
				}

			runResp, err := client.RunWorkload(ctx, &runtimeapi.RunRequest{
				Id:           id,
				RootDigest:   pullResp.RootDigest,
				Backend:      "container",
				Command:      svc.Command,
				Env:          env,
				NetworkRules: networkRules,
				NetworkMode:  "bridge",
				WorkingDir:   workingDir,
				BridgeName:   bridgeName,
			})
				if err != nil {
					fmt.Fprintf(os.Stderr, "\n  %s: START FAILED: %v\n", name, err)
					return fmt.Errorf("run %s: %w", name, err)
				}

				fmt.Printf("OK (id=%s, ip=%s)\n", runResp.Id, runResp.InternalIp)
			}

			fmt.Println("Done.")
			return nil
		},
	}
}
