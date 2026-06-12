//go:build windows

package cmd

import (
	"google.golang.org/grpc"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

func setupRawTerminal() (func(), error) {
	return nil, nil
}

func watchWindowSize(stream grpc.BidiStreamingClient[runtimepb.AttachMessage, runtimepb.AttachMessage]) {}
