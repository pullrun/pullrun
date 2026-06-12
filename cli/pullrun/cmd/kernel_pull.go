// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"archive/tar"
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/klauspost/compress/zstd"
	"github.com/spf13/cobra"
)

// NewKernelCommand returns the parent `pullrun kernel` command.
//
// Subcommands:
//   - install: download a pre-built Linux kernel for Apple Virt VMs
//     from Kata Containers' official releases.
//
// The installed kernel is the "user-mode" image Apple uses
// internally for `container` (Apple's CLI on top of the
// Apple Virtualization framework). The container team's
// default is at `/opt/kata/share/kata-containers/vmlinux.container`
// (extracted from the `kata-static-<ver>-<arch>.tar.zst`
// release artifact). We mirror that strategy: download the
// tarball, extract `vmlinux.container`, and place it at
// `~/.pullrun/kernels/vmlinux-<ver>` by default.
//
// Apple also supports user-provided kernels (starting with
// Linux 6.14.9 on arm64); pass `--from <path>` to use a
// kernel you built yourself.
func NewKernelCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "kernel",
		Short: "Manage Linux kernel images for VM-backed workloads",
	}
	cmd.AddCommand(NewKernelInstallCommand(opts))
	return cmd
}

func NewKernelInstallCommand(opts *RootOptions) *cobra.Command {
	var (
		version     string
		arch        string
		dest        string
		fromLocal   string
		skipVerify  bool
		registryURL string
	)

	cmd := &cobra.Command{
		Use:   "install",
		Short: "Install a Linux kernel image for Apple Virt VMs",
		Long: `Download (or copy) a Linux kernel image suitable for booting
Apple Silicon microVMs. The default source is Kata Containers'
official releases; the default destination is
~/.pullrun/kernels/vmlinux-<version>.

Once installed, point pullrun at the kernel via the
PULLRUN_KERNEL_PATH environment variable when starting
pullrun-runtime, or pass --kernel-path to the smoke binary.

Examples:
  # Download Kata's arm64 kernel for Linux 3.31.0
  pullrun kernel install --version 3.31.0

  # Copy a kernel you already have on disk
  pullrun kernel install --from /path/to/vmlinux.container

  # Install Kata's amd64 kernel on an Intel Mac
  pullrun kernel install --version 3.31.0 --arch amd64`,
		RunE: func(cmd *cobra.Command, args []string) error {
			arch = normalizeArch(arch)

			destPath := dest
			if destPath == "" {
				home, err := os.UserHomeDir()
				if err != nil {
					return fmt.Errorf("resolve $HOME: %w", err)
				}
				if fromLocal != "" {
					destPath = filepath.Join(home, ".pullrun", "kernels", filepath.Base(fromLocal))
				} else {
					destPath = filepath.Join(home, ".pullrun", "kernels", fmt.Sprintf("vmlinux-%s", version))
				}
			}
			if err := os.MkdirAll(filepath.Dir(destPath), 0o755); err != nil {
				return fmt.Errorf("create dest dir: %w", err)
			}

			if fromLocal != "" {
				return copyLocal(fromLocal, destPath)
			}

			if version == "" {
				return fmt.Errorf("--version is required (or pass --from <local path>)")
			}

			url := buildKataURL(registryURL, version, arch)
			fmt.Fprintf(cmd.OutOrStdout(), "Downloading Kata Containers static tarball...\n  version: %s\n  arch:    %s\n  url:     %s\n", version, arch, url)

			tarballPath, err := downloadFile(cmd.Context(), url, "kata-static.tar.zst")
			if err != nil {
				return err
			}
			defer os.Remove(tarballPath)

			vmlinuxPath, err := extractVmlinuxFromTarball(cmd.Context(), tarballPath, destPath)
			if err != nil {
				return err
			}

			if !skipVerify {
				if err := verifyVmlinux(vmlinuxPath); err != nil {
					return fmt.Errorf("verify installed kernel: %w", err)
				}
			}

			fmt.Fprintf(cmd.OutOrStdout(), "\nInstalled kernel: %s\n", vmlinuxPath)
			fmt.Fprintf(cmd.OutOrStdout(), "\nTo use it, set the env var when starting pullrun-runtime:\n  PULLRUN_KERNEL_PATH=%s pullrun-runtime ...\n", vmlinuxPath)
			return nil
		},
	}

	cmd.Flags().StringVar(&version, "version", "3.31.0", "Kata Containers release version to download")
	cmd.Flags().StringVar(&arch, "arch", "arm64", "Architecture: arm64 (Apple Silicon) or amd64 (Intel Mac, untested)")
	cmd.Flags().StringVar(&dest, "dest", "", "Destination file path (default: ~/.pullrun/kernels/vmlinux-<ver>)")
	cmd.Flags().StringVar(&fromLocal, "from", "", "Use a local kernel file instead of downloading")
	cmd.Flags().BoolVar(&skipVerify, "no-verify", false, "Skip post-install sanity check (vmlinux magic header)")
	cmd.Flags().StringVar(&registryURL, "registry", "https://github.com/kata-containers/kata-containers/releases/download", "Base URL for Kata releases")
	return cmd
}

func normalizeArch(s string) string {
	switch strings.ToLower(s) {
	case "arm64", "aarch64":
		return "arm64"
	case "amd64", "x86_64", "x64":
		return "amd64"
	default:
		return s
	}
}

func buildKataURL(base, version, arch string) string {
	return fmt.Sprintf("%s/%s/kata-static-%s-%s.tar.zst", base, version, version, arch)
}

func copyLocal(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open source: %w", err)
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
	if err != nil {
		return fmt.Errorf("open dest: %w", err)
	}
	defer out.Close()
	if _, err := io.Copy(out, in); err != nil {
		return fmt.Errorf("copy: %w", err)
	}
	fmt.Fprintf(os.Stderr, "Copied %s -> %s\n", src, dst)
	return nil
}

func downloadFile(ctx context.Context, url, suggestedName string) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", fmt.Errorf("build request: %w", err)
	}
	client := &http.Client{Timeout: 10 * time.Minute}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("download: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("download: HTTP %d for %s", resp.StatusCode, url)
	}
	f, err := os.CreateTemp("", suggestedName+".*")
	if err != nil {
		return "", fmt.Errorf("create temp: %w", err)
	}
	defer f.Close()
	if _, err := io.Copy(f, resp.Body); err != nil {
		os.Remove(f.Name())
		return "", fmt.Errorf("write tarball: %w", err)
	}
	return f.Name(), nil
}

// extractVmlinuxFromTarball finds the vmlinux* file inside a
// zstd-compressed tarball and writes it to `dest`. Returns
// the absolute path of the installed kernel.
//
// Kata's `kata-static-*.tar.zst` layout:
//
//	opt/kata/share/kata-containers/vmlinux.container
//	opt/kata/bin/kata-runtime
//	...
//
// We pick the first file whose basename starts with
// `vmlinux` and has no extension (it's an ELF).
func extractVmlinuxFromTarball(ctx context.Context, tarballPath, dest string) (string, error) {
	f, err := os.Open(tarballPath)
	if err != nil {
		return "", fmt.Errorf("open tarball: %w", err)
	}
	defer f.Close()
	zr, err := zstd.NewReader(f)
	if err != nil {
		return "", fmt.Errorf("zstd reader: %w", err)
	}
	defer zr.Close()
	tr := tar.NewReader(zr)
	for {
		if err := ctx.Err(); err != nil {
			return "", err
		}
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return "", fmt.Errorf("tar next: %w", err)
		}
		if hdr.Typeflag != tar.TypeReg {
			continue
		}
		base := filepath.Base(hdr.Name)
		if !strings.HasPrefix(base, "vmlinux") {
			continue
		}
		if strings.HasSuffix(base, ".txt") || strings.HasSuffix(base, ".md") {
			continue
		}
		out, err := os.OpenFile(dest, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
		if err != nil {
			return "", fmt.Errorf("open dest: %w", err)
		}
		if _, err := io.Copy(out, tr); err != nil {
			out.Close()
			return "", fmt.Errorf("extract: %w", err)
		}
		if err := out.Close(); err != nil {
			return "", fmt.Errorf("close dest: %w", err)
		}
		return dest, nil
	}
	return "", fmt.Errorf("no vmlinux* file found in tarball %s", tarballPath)
}

// verifyVmlinux checks the magic header at offset 0x0: ELF
// (0x7F 'E' 'L' 'F'). It's a sanity check, not a signature
// verification.
func verifyVmlinux(path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	var magic [4]byte
	if _, err := io.ReadFull(f, magic[:]); err != nil {
		return fmt.Errorf("read magic: %w", err)
	}
	if magic != [4]byte{0x7F, 'E', 'L', 'F'} {
		return fmt.Errorf("not an ELF file (got %v)", magic)
	}
	return nil
}
