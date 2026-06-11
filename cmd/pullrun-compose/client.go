package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	runtimeapi "pullrun/protoapi/pullrun/runtime"
)

func defaultSocketPath() string {
	sock := os.Getenv("PULLRUN_SOCKET")
	if sock != "" {
		return sock
	}
	return filepath.Join(os.TempDir(), "pullrun.sock")
}

func connectRuntime(ctx context.Context) (runtimeapi.RuntimeClient, *grpc.ClientConn, error) {
	sock := defaultSocketPath()
	conn, err := grpc.NewClient(
		"unix://"+sock,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		return nil, nil, fmt.Errorf("connect to %s: %w", sock, err)
	}
	return runtimeapi.NewRuntimeClient(conn), conn, nil
}
