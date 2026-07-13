// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"fmt"
	"os"
	"os/exec"

	"github.com/spf13/cobra"
)

func NewComposeCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:                "compose",
		Short:              "Run Compose workloads (delegates to pullrun-compose binary)",
		DisableFlagParsing: true,
		RunE: func(cmd *cobra.Command, args []string) error {
			// Find pullrun-compose on PATH, or next to the pullrun binary.
			composeBin := "pullrun-compose"
			if _, err := exec.LookPath(composeBin); err != nil {
				// Fall back to same directory as the pullrun binary.
				exe, err := os.Executable()
				if err != nil {
					return fmt.Errorf("pullrun-compose not found on PATH and cannot detect executable dir")
				}
				dir := exe
				if len(dir) > 0 && dir[0] == '/' {
					// Absolute path — look in the same directory.
					composeBin = dir[:len(dir)-len("pullrun")] + "pullrun-compose"
				}
				if _, err := os.Stat(composeBin); err != nil {
					return fmt.Errorf("pullrun-compose not found on PATH or next to pullrun binary; install separately")
				}
			}

			c := exec.Command(composeBin, args...)
			c.Stdin = os.Stdin
			c.Stdout = os.Stdout
			c.Stderr = os.Stderr
			if err := c.Run(); err != nil {
				if exit, ok := err.(*exec.ExitError); ok {
					os.Exit(exit.ExitCode())
				}
				return fmt.Errorf("pullrun-compose: %w", err)
			}
			return nil
		},
	}
	return cmd
}
