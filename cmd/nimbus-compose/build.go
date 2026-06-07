package main

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"github.com/spf13/cobra"

	runtimeapi "nimbus/protoapi/nimbus/runtime"
)

func newBuildCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "build [service...]",
		Short: "Build or rebuild services",
		Long: `Build OCI images for services that have a 'build:' section in the compose file.
Delegates to 'docker build' and then imports the image into the nimbus runtime.

If no service names are given, all services with build sections are built.`,
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

			// Determine which services to build
			names := args
			if len(names) == 0 {
				for name, svc := range project.Services {
					if svc.Build != nil {
						names = append(names, name)
					}
				}
			}

			if len(names) == 0 {
				fmt.Println("No services have a 'build:' section.")
				return nil
			}

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
			defer cancel()

			client, conn, err := connectRuntime(ctx)
			if err != nil {
				return fmt.Errorf("connect to nimbus runtime: %w", err)
			}
			defer conn.Close()

			// Check that docker is available
			if _, err := exec.LookPath("docker"); err != nil {
				return fmt.Errorf("docker not found in PATH (required for build): %w", err)
			}

			workingDir := filepath.Dir(filePath)

			for _, name := range names {
				svc, ok := project.Services[name]
				if !ok {
					return fmt.Errorf("service %q not found", name)
				}
				if svc.Build == nil {
					fmt.Printf("  %s: no build section, skipping\n", name)
					continue
				}

				tag := fmt.Sprintf("%s-%s:latest", project.Name, name)
				contextDir := svc.Build.Context
				if !filepath.IsAbs(contextDir) {
					contextDir = filepath.Join(workingDir, contextDir)
				}

				// Determine Dockerfile path
				dockerfile := svc.Build.Dockerfile
				if dockerfile == "" {
					dockerfile = filepath.Join(contextDir, "Dockerfile")
				} else if !filepath.IsAbs(dockerfile) {
					dockerfile = filepath.Join(contextDir, dockerfile)
				}

				fmt.Printf("  %s: building (context=%s, dockerfile=%s)...\n",
					name, contextDir, dockerfile)

				buildArgs := []string{"build", "-t", tag, "-f", dockerfile, contextDir}

				// Add build args if present
				for k, v := range svc.Build.Args {
					if v != nil {
						buildArgs = append(buildArgs, "--build-arg", fmt.Sprintf("%s=%s", k, *v))
					}
				}

				buildCmd := exec.CommandContext(ctx, "docker", buildArgs...)
				buildCmd.Stdout = os.Stdout
				buildCmd.Stderr = os.Stderr

				if err := buildCmd.Run(); err != nil {
					return fmt.Errorf("docker build %s: %w", name, err)
				}

				fmt.Printf("  %s: importing into nimbus runtime...\n", name)

				pullResp, err := client.PullImage(ctx, &runtimeapi.PullImageRequest{
					ImageRef: tag,
					Registry: "",
				})
				if err != nil {
					return fmt.Errorf("import %s into runtime: %w", name, err)
				}

				_ = pullResp
				fmt.Printf("  %s: built and imported (digest=%s)\n", name, pullResp.RootDigest)

				// Update the service's image to use the built tag
				svc.Image = tag
				project.Services[name] = svc
			}

			fmt.Println("Done.")
			return nil
		},
	}
}
