// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"fmt"
	"os"
	"os/exec"
	"strings"

	"github.com/spf13/cobra"
)

// pullrun global flags that must be stripped before forwarding args to
// pullrun-compose. The bool indicates whether the flag takes a value as the
// next argument.
var globalFlags = map[string]bool{
	"--direct": false,
	"--socket": true,
	"--server": true,
}

// filterGlobalFlags removes pullrun global flags and their values from args,
// preventing them from leaking through to the pullrun-compose binary.
func filterGlobalFlags(args []string) []string {
	var out []string
	for i := 0; i < len(args); i++ {
		arg := args[i]
		takesValue, isFlag := globalFlags[arg]
		if isFlag {
			if takesValue && i+1 < len(args) {
				i++
			}
			continue
		}
		// Also handle --flag=value form.
		hasPrefix := false
		for flag := range globalFlags {
			if strings.HasPrefix(arg, flag+"=") {
				hasPrefix = true
				break
			}
		}
		if hasPrefix {
			continue
		}
		out = append(out, arg)
	}
	return out
}

func NewComposeCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:                "compose",
		Short:              "Run Compose workloads (delegates to pullrun-compose binary)",
		DisableFlagParsing: true,
		RunE: func(cmd *cobra.Command, args []string) error {
			// Find pullrun-compose on PATH, or next to the pullrun binary.
			composeBin := "pullrun-compose"
			if _, err := exec.LookPath(composeBin); err != nil {
				exe, err := os.Executable()
				if err != nil {
					return fmt.Errorf("pullrun-compose not found on PATH and cannot detect executable dir")
				}
				dir := exe
				if len(dir) > 0 && dir[0] == '/' {
					composeBin = dir[:len(dir)-len("pullrun")] + "pullrun-compose"
				}
				if _, err := os.Stat(composeBin); err != nil {
					return fmt.Errorf("pullrun-compose not found on PATH or next to pullrun binary; install separately")
				}
			}

			c := exec.Command(composeBin, filterGlobalFlags(args)...)
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
