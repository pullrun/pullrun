// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//go:build unix || linux || darwin

package cmd

import (
	"os"
	"os/signal"
	"syscall"
	"time"

	"google.golang.org/grpc"
	"golang.org/x/term"
	"golang.org/x/sys/unix"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

func setupRawTerminal() (func(), error) {
	oldState, err := term.MakeRaw(int(os.Stdin.Fd()))
	if err != nil {
		return nil, err
	}

	if tios, err := unix.IoctlGetTermios(int(os.Stdin.Fd()), termiosGetReq); err == nil {
		tios.Oflag |= unix.OPOST | unix.ONLCR
		_ = unix.IoctlSetTermios(int(os.Stdin.Fd()), termiosSetReq, tios)
	}

	return func() {
		buf := make([]byte, 1024)
		_ = os.Stdin.SetReadDeadline(time.Now().Add(50 * time.Millisecond))
		for {
			_, err := os.Stdin.Read(buf)
			if err != nil {
				break
			}
		}
		_ = os.Stdin.SetReadDeadline(time.Time{})
		_ = term.Restore(int(os.Stdin.Fd()), oldState)
	}, nil
}

func watchWindowSize(stream grpc.BidiStreamingClient[runtimepb.AttachMessage, runtimepb.AttachMessage]) {
	winCh := make(chan os.Signal, 1)
	signal.Notify(winCh, syscall.SIGWINCH)
	go func() {
		sendWinSize := func() {
			w, h, err := term.GetSize(int(os.Stdin.Fd()))
			if err != nil {
				return
			}
			_ = stream.Send(&runtimepb.AttachMessage{
				Body: &runtimepb.AttachMessage_WindowSize{
					WindowSize: &runtimepb.AttachWindowSize{
						Rows: uint32(h),
						Cols: uint32(w),
					},
				},
			})
		}
		sendWinSize()
		for range winCh {
			sendWinSize()
		}
	}()
}
