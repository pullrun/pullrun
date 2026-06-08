package cmd

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

func NewLoginCommand(opts *RootOptions) *cobra.Command {
	var username, password, token string

	cmd := &cobra.Command{
		Use:   "login [REGISTRY]",
		Short: "Authenticate to a container registry",
		Long: `Authenticate to a container registry and store credentials.

REGISTRY is the registry hostname (e.g. "docker.io", "ghcr.io",
"registry.example.com:5000"). Defaults to "docker.io" if omitted.

Credentials are stored in ~/.nimbus/auth.json with 0600 permissions.
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
				password = string(data)
			}

			if username == "" && password == "" && token == "" {
				return fmt.Errorf("credentials required: use --username/--password, --password-stdin, or --token")
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
