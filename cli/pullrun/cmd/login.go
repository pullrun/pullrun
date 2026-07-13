// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"bufio"
	"fmt"
	"net/http"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"golang.org/x/term"
)

func NewLoginCommand(opts *RootOptions) *cobra.Command {
	var username, password, token string

	cmd := &cobra.Command{
		Use:   "login [REGISTRY]",
		Short: "Authenticate to a container registry",
		Long: `Authenticate to a container registry and store credentials.

REGISTRY is the registry hostname (e.g. "docker.io", "ghcr.io",
"registry.example.com:5000"). Defaults to "docker.io" if omitted.

Credentials are stored in ~/.pullrun/auth.json with 0600 permissions.
Use '--password-stdin' to read the password from stdin securely.`,
		Args: cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			registry := "docker.io"
			if len(args) > 0 {
				registry = args[0]
			}
			registry = NormalizeRegistry(registry)

			passwordStdin, _ := cmd.Flags().GetBool("password-stdin")
			if passwordStdin {
				data, err := os.ReadFile("/dev/stdin")
				if err != nil {
					return fmt.Errorf("read stdin: %w", err)
				}
				password = strings.TrimRight(string(data), "\n\r")
			}

			// Interactive prompt if no credentials provided via flags.
			if username == "" && password == "" && token == "" {
				reader := bufio.NewReader(os.Stdin)
				fmt.Printf("Username: ")
				u, _ := reader.ReadString('\n')
				username = strings.TrimRight(u, "\n\r")

				fmt.Printf("Password: ")
				bytePw, err := term.ReadPassword(int(os.Stdin.Fd()))
				fmt.Println()
				if err != nil {
					return fmt.Errorf("read password: %w", err)
				}
				password = string(bytePw)
			}

			if username == "" && password == "" && token == "" {
				return fmt.Errorf("credentials required: use --username/--password, --password-stdin, or --token")
			}

			// Validate credentials by pinging the registry's /v2/ endpoint.
			pingURL := fmt.Sprintf("https://%s/v2/", registry)
			req, err := http.NewRequest("GET", pingURL, nil)
			if err != nil {
				return fmt.Errorf("create ping request: %w", err)
			}
			req.Header.Set("User-Agent", "pullrun")
			if token != "" {
				req.Header.Set("Authorization", "Bearer "+token)
			} else if username != "" || password != "" {
				req.SetBasicAuth(username, password)
			}

			resp, err := http.DefaultClient.Do(req)
			if err != nil {
				return fmt.Errorf("ping registry %s: %w", registry, err)
			}
			resp.Body.Close()

			if resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden {
				return fmt.Errorf("authentication failed for %s (HTTP %d)", registry, resp.StatusCode)
			}
			if resp.StatusCode < 200 || resp.StatusCode >= 300 {
				return fmt.Errorf("registry %s returned HTTP %d (expected 2xx)", registry, resp.StatusCode)
			}

			auth := RegistryAuth{
				Username: username,
				Password: password,
				Token:    token,
			}
			if err := SetRegistryAuth(registry, auth); err != nil {
				return fmt.Errorf("save credentials: %w", err)
			}

			fmt.Printf("✓ logged in to %s\n", registry)
			return nil
		},
	}

	cmd.Flags().StringVarP(&username, "username", "u", "", "Registry username")
	cmd.Flags().StringVarP(&password, "password", "p", "", "Registry password (use --password-stdin for security)")
	cmd.Flags().StringVar(&token, "token", "", "Registry token (alternative to username/password)")
	cmd.Flags().Bool("password-stdin", false, "Read password from stdin")
	return cmd
}

func NewLogoutCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "logout [REGISTRY]",
		Short: "Remove stored registry credentials",
		Long: `Remove stored credentials for a registry.

REGISTRY is the registry hostname (e.g. "docker.io", "ghcr.io").
Defaults to "docker.io" if omitted.`,
		Args: cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			registry := "docker.io"
			if len(args) > 0 {
				registry = args[0]
			}
			registry = NormalizeRegistry(registry)

			if err := RemoveRegistryAuth(registry); err != nil {
				return fmt.Errorf("remove credentials: %w", err)
			}

			fmt.Printf("✓ logged out of %s\n", registry)
			return nil
		},
	}
	return cmd
}
