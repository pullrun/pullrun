package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

var composeFile string
var verbose bool

func main() {
	rootCmd := &cobra.Command{
		Use:   "pullrun-compose",
		Short: "Pullrun Compose — run Compose workloads as micro-VMs",
		Long: `pullrun-compose reads a docker-compose YAML file and boots each service
as an isolated micro-VM via the pullrun runtime. It is the zero-migration
path from Docker Compose to micro-VM isolation.

Examples:
  pullrun-compose up                    Start all services from compose.yaml
  pullrun-compose down                  Stop all services
  pullrun-compose ps                    List service status
  pullrun-compose logs                  Show logs from all services
  pullrun-compose up -f my-app.yaml     Use a specific compose file`,
	}

	rootCmd.PersistentFlags().StringVarP(&composeFile, "file", "f", "", "Path to compose file (default: compose.yaml)")
	rootCmd.PersistentFlags().BoolVarP(&verbose, "verbose", "v", false, "Verbose output")

	rootCmd.AddCommand(newUpCommand())
	rootCmd.AddCommand(newDownCommand())
	rootCmd.AddCommand(newPsCommand())
	rootCmd.AddCommand(newLogsCommand())
	rootCmd.AddCommand(newBuildCommand())

	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
}

func resolveComposeFile() string {
	if composeFile != "" {
		return composeFile
	}
	for _, name := range []string{"compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"} {
		if _, err := os.Stat(name); err == nil {
			return name
		}
	}
	return "compose.yaml"
}
