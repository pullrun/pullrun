// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//go:build unix || linux || darwin

package cmd

import (
	"os"
	"os/signal"
	"syscall"

	"golang.org/x/sys/unix"
	"golang.org/x/term"
	"google.golang.org/grpc"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

func setupRawTerminal() (func(), error) {
	fd := int(os.Stdin.Fd())

	oldState, err := unix.IoctlGetTermios(fd, termiosGetReq)
	if err != nil {
		return nil, err
	}

	raw := *oldState
	raw.Iflag &^= unix.IGNBRK | unix.BRKINT | unix.PARMRK | unix.ISTRIP | unix.INLCR | unix.IGNCR | unix.ICRNL | unix.IXON
	raw.Oflag |= unix.OPOST | unix.ONLCR
	raw.Lflag &^= unix.ECHO | unix.ECHONL | unix.ICANON | unix.ISIG | unix.IEXTEN
	raw.Cflag &^= unix.CSIZE | unix.PARENB
	raw.Cflag |= unix.CS8
	raw.Cc[unix.VMIN] = 1
	raw.Cc[unix.VTIME] = 0

	if err := unix.IoctlSetTermios(fd, termiosSetReq, &raw); err != nil {
		return nil, err
	}

	return func() {
		_ = unix.IoctlSetTermios(fd, termiosSetReq, oldState)
		os.Stderr.WriteString("\r")
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
