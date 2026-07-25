// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"github.com/spf13/cobra"
)

type RootOptions struct {
	SocketPath string
	ServerAddr string
	DirectMode bool
}

func NewRootCommand() *cobra.Command {
	opts := &RootOptions{}

	cmd := &cobra.Command{
		Use:   "pullrun",
		Short: "Pullrun workload CLI",
		Long: `pullrun manages Pullrun workloads - pull images, run containers/VMs,
and inspect network policies. All communication with the runtime uses gRPC
over a Unix domain socket.`,
		Version: "0.7.4",
	}

	cmd.Flags().BoolP("version", "V", false, "version for pullrun")

	cmd.PersistentFlags().StringVar(&opts.SocketPath, "socket", DefaultSocketPath, "Runtime socket path (UDS on Unix, TCP host:port on Windows)")
	cmd.PersistentFlags().StringVar(&opts.ServerAddr, "server", "", "Control plane address (disables direct mode)")
	cmd.PersistentFlags().BoolVar(&opts.DirectMode, "direct", true, "Use direct mode (spawn runtime as child)")

	cmd.AddCommand(NewPullCommand(opts))
	cmd.AddCommand(NewRunCommand(opts))
	cmd.AddCommand(NewStopCommand(opts))
	cmd.AddCommand(NewListCommand(opts))
	cmd.AddCommand(NewGetCommand(opts))
	cmd.AddCommand(NewExecCommand(opts))
	cmd.AddCommand(NewLogsCommand(opts))
	cmd.AddCommand(NewInspectCommand(opts))
	cmd.AddCommand(NewEventsCommand(opts))
	cmd.AddCommand(NewWorkloadCommand(opts))
	cmd.AddCommand(NewKernelCommand(opts))
	cmd.AddCommand(NewBuildCommand(opts))
	cmd.AddCommand(NewPushCommand(opts))
	cmd.AddCommand(NewSaveCommand(opts))
	cmd.AddCommand(NewLoadCommand(opts))
	cmd.AddCommand(NewLoginCommand(opts))
	cmd.AddCommand(NewLogoutCommand(opts))
	cmd.AddCommand(NewUpdateCommand(opts))
	cmd.AddCommand(NewStatsCommand(opts))
	cmd.AddCommand(NewCpCommand(opts))
	cmd.AddCommand(NewCommitCommand(opts))
	cmd.AddCommand(NewDiffCommand(opts))
	cmd.AddCommand(NewNetworkCommand(opts))
	cmd.AddCommand(NewSecretCommand(opts))
	cmd.AddCommand(NewConfigCommand(opts))
	cmd.AddCommand(NewPruneCommand(opts))
	cmd.AddCommand(NewImagesCommand(opts))
	cmd.AddCommand(NewRmiCommand(opts))
	cmd.AddCommand(NewGCCommand(opts))
	cmd.AddCommand(NewInfoCommand(opts))
	cmd.AddCommand(NewMCPCommand(opts))
	cmd.AddCommand(NewVersionCommand())
	cmd.AddCommand(NewComposeCommand(opts))

	return cmd
}
