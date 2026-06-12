// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/spf13/cobra"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

func NewBuildCommand(opts *RootOptions) *cobra.Command {
	var platform string

	cmd := &cobra.Command{
		Use:   "build [DOCKERFILE] [CONTEXT] -t TAG",
		Short: "Build an OCI image from a Dockerfile using native DAG builder",
		Args:  cobra.RangeArgs(0, 2),
		Long: `Build an OCI image from a Dockerfile using the native DAG-aware builder.

Parses the Dockerfile, pulls the base image, executes RUN instructions
directly via runc, and snapshots each layer into the DAG store — no
Docker required.

Multi-platform builds: pass --platform linux/amd64,linux/arm64 to build
for multiple architectures. The result is a manifest list stored in the
DAG. Combine with --push to push the manifest list to a registry.

Args:
  DOCKERFILE   path to Dockerfile (default: "./Dockerfile")
  CONTEXT      build context directory (default: directory of Dockerfile)`,
		RunE: func(cmd *cobra.Command, args []string) error {
			dockerfile := "./Dockerfile"
			contextDir := "."
			if len(args) >= 1 {
				dockerfile = args[0]
			}
			if len(args) >= 2 {
				contextDir = args[1]
			}

			tag, _ := cmd.Flags().GetString("tag")
			push, _ := cmd.Flags().GetBool("push")
			buildArgs, _ := cmd.Flags().GetStringToString("build-arg")

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			// Parse comma-separated platforms into primary + multi slice.
			primaryPlatform, multiPlatforms := splitPlatforms(platform)

			resp, err := client.BuildImage(ctx, &runtimepb.BuildImageRequest{
				Dockerfile: dockerfile,
				ContextDir: contextDir,
				Tag:        tag,
				BuildArgs:  buildArgs,
				Platform:   primaryPlatform,
				Platforms:  multiPlatforms,
				Push:       push,
			})
			if err != nil {
				return fmt.Errorf("build: %w", err)
			}

			fmt.Printf("✓ built %s\n", dockerfile)
			fmt.Printf("  root digest: %s\n", resp.RootDigest)
			if resp.Tag != "" {
				fmt.Printf("  tag:         %s\n", resp.Tag)
			}
			if push {
				fmt.Printf("  pushed to    %s\n", tag)
			}
			return nil
		},
	}
	cmd.Flags().StringP("tag", "t", "", "Image tag (e.g. registry.example.com/myapp:latest)")
	cmd.Flags().StringToString("build-arg", nil, "Build arguments (KEY=VALUE)")
	cmd.Flags().StringVar(&platform, "platform", "", "Target platform (e.g. linux/amd64, linux/arm64); comma-separated for multi-arch")
	cmd.Flags().Bool("push", false, "Push the built image to the registry after building")
	return cmd
}

// splitPlatforms parses a comma-separated platform string.
// Returns the primary single platform and a slice for multi-platform.
func splitPlatforms(raw string) (primary string, multi []string) {
	if raw == "" {
		return "", nil
	}
	parts := splitAndTrim(raw)
	if len(parts) == 1 {
		return parts[0], nil
	}
	return "", parts
}

func splitAndTrim(s string) []string {
	var out []string
	for _, p := range strings.Split(s, ",") {
		p = strings.TrimSpace(p)
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

func NewPushCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "push [ROOT_DIGEST] [TARGET_REF]",
		Short: "Push a DAG image to an OCI registry",
		Args:  cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			// Extract registry host from target ref for credential lookup.
			registry := extractRegistryFromRef(args[1])
			auth, _ := GetRegistryAuth(NormalizeRegistry(registry))

			resp, err := client.PushImage(ctx, args[0], args[1], auth)
			if err != nil {
				return fmt.Errorf("push: %w", err)
			}

			fmt.Printf("✓ pushed to %s\n", args[1])
			fmt.Printf("  manifest digest: %s\n", resp.ManifestDigest)
			fmt.Printf("  bytes pushed:    %d\n", resp.BytesPushed)
			return nil
		},
	}
	return cmd
}

func NewSaveCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "save [ROOT_DIGEST] -o OUTPUT.tar",
		Short: "Export a DAG image to a tar archive",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			output, _ := cmd.Flags().GetString("output")
			if output == "" {
				return fmt.Errorf("--output is required")
			}
			format, _ := cmd.Flags().GetString("format")
			if format == "" {
				format = "dag"
			}

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			stream, err := client.ExportImage(ctx, args[0], format)
			if err != nil {
				return fmt.Errorf("save: %w", err)
			}

			f, err := os.Create(output)
			if err != nil {
				return fmt.Errorf("create %s: %w", output, err)
			}
			defer f.Close()

			var total int64
			for {
				chunk, err := stream.Recv()
				if err == io.EOF {
					break
				}
				if err != nil {
					return fmt.Errorf("save stream: %w", err)
				}
				n, err := f.Write(chunk.Data)
				if err != nil {
					return fmt.Errorf("write %s: %w", output, err)
				}
				total += int64(n)
			}

			fmt.Printf("✓ saved to %s (%d bytes)\n", output, total)
			return nil
		},
	}
	cmd.Flags().StringP("output", "o", "", "Output file path (required)")
	cmd.Flags().String("format", "dag", "Export format: dag or oci")
	return cmd
}

func NewLoadCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "load -i INPUT.tar",
		Short: "Import a DAG image from a tar archive",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			input, _ := cmd.Flags().GetString("input")
			if input == "" {
				return fmt.Errorf("--input is required")
			}

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Minute)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			stream, err := client.ImportImage(ctx)
			if err != nil {
				return fmt.Errorf("load: %w", err)
			}

			f, err := os.Open(input)
			if err != nil {
				return fmt.Errorf("open %s: %w", input, err)
			}
			defer f.Close()

			buf := make([]byte, 64*1024)
			var total int64
			for {
				n, err := f.Read(buf)
				if n > 0 {
					if err := stream.Send(&runtimepb.ImportImageChunk{Data: buf[:n]}); err != nil {
						return fmt.Errorf("load send: %w", err)
					}
					total += int64(n)
				}
				if err == io.EOF {
					break
				}
				if err != nil {
					return fmt.Errorf("read %s: %w", input, err)
				}
			}

			resp, err := stream.CloseAndRecv()
			if err != nil {
				return fmt.Errorf("load: %w", err)
			}

			fmt.Printf("✓ loaded from %s\n", input)
			fmt.Printf("  root digest:    %s\n", resp.RootDigest)
			fmt.Printf("  bytes stored:   %d\n", resp.BytesStored)
			if resp.BytesDeduplicated > 0 {
				fmt.Printf("  bytes deduped:  %d\n", resp.BytesDeduplicated)
			}
			return nil
		},
	}
	cmd.Flags().StringP("input", "i", "", "Input tar file path (required)")
	return cmd
}
