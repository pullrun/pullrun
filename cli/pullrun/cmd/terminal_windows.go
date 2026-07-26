// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//go:build windows

package cmd

import (
	"os"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
	"google.golang.org/grpc"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

var (
	kernel32               = windows.NewLazySystemDLL("kernel32.dll")
	procGetConsoleMode     = kernel32.NewProc("GetConsoleMode")
	procSetConsoleMode     = kernel32.NewProc("SetConsoleMode")
	procGetConsoleCP       = kernel32.NewProc("GetConsoleCP")
	procSetConsoleCP       = kernel32.NewProc("SetConsoleCP")
	procGetConsoleOutputCP = kernel32.NewProc("GetConsoleOutputCP")
	procSetConsoleOutputCP = kernel32.NewProc("SetConsoleOutputCP")
)

const (
	enableVirtualTerminalProcessing = 0x0004
	enableVirtualTerminalInput      = 0x0200
	enableProcessedOutput           = 0x0001
	enableWrapAtEolOutput           = 0x0002
	disableNewlineAutoReturn        = 0x0008
)

func getConsoleMode(handle windows.Handle) (uint32, error) {
	var mode uint32
	r, _, err := procGetConsoleMode.Call(uintptr(handle), uintptr(unsafe.Pointer(&mode)))
	if r == 0 {
		return 0, err
	}
	return mode, nil
}

func setConsoleMode(handle windows.Handle, mode uint32) error {
	r, _, err := procSetConsoleMode.Call(uintptr(handle), uintptr(mode))
	if r == 0 {
		return err
	}
	return nil
}

func setupRawTerminal() (func(), error) {
	inHandle := windows.Handle(os.Stdin.Fd())

	oldInMode, err := getConsoleMode(inHandle)
	if err != nil {
		return nil, nil // not a console
	}

	// Enable virtual terminal processing for escape sequences
	newInMode := oldInMode | enableVirtualTerminalProcessing | enableVirtualTerminalInput
	// Disable line input, echo, and processed input for raw mode
	newInMode &^= 0x0002 | 0x0004 | 0x0001 // ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT

	if err := setConsoleMode(inHandle, newInMode); err != nil {
		return nil, nil
	}

	oldCp, _, _ := procGetConsoleCP.Call()
	oldOutputCp, _, _ := procGetConsoleOutputCP.Call()
	procSetConsoleCP.Call(65001)       // UTF-8 input
	procSetConsoleOutputCP.Call(65001) // UTF-8 output

	return func() {
		setConsoleMode(inHandle, oldInMode)
		if oldCp != 0 {
			procSetConsoleCP.Call(oldCp)
		}
		if oldOutputCp != 0 {
			procSetConsoleOutputCP.Call(oldOutputCp)
		}
		buf := make([]byte, 1024)
		_ = os.Stdin.SetReadDeadline(time.Now().Add(50 * time.Millisecond))
		for {
			_, err := os.Stdin.Read(buf)
			if err != nil {
				break
			}
		}
		_ = os.Stdin.SetReadDeadline(time.Time{})
	}, nil
}

func watchWindowSize(stream grpc.BidiStreamingClient[runtimepb.AttachMessage, runtimepb.AttachMessage], stop <-chan struct{}) {
	// Windows console doesn't send SIGWINCH. Poll terminal size periodically.
	go func() {
		lastW, lastH := 0, 0
		ticker := time.NewTicker(2 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
			w, h, err := getConsoleSize()
			if err != nil {
				continue
			}
			if w != lastW || h != lastH {
				lastW, lastH = w, h
				_ = stream.Send(&runtimepb.AttachMessage{
					Body: &runtimepb.AttachMessage_WindowSize{
						WindowSize: &runtimepb.AttachWindowSize{
							Rows: uint32(h),
							Cols: uint32(w),
						},
					},
				})
			}
			case <-stop:
				return
			}
		}
	}()
}

func getConsoleSize() (int, int, error) {
	var info struct {
		dwSize, dwCursorPosition struct{ X, Y int16 }
		wAttributes              uint16
		srWindow                 struct{ Left, Top, Right, Bottom int16 }
		dwMaximumWindowSize      struct{ X, Y int16 }
	}
	handle := windows.Handle(os.Stdout.Fd())
	r, _, err := windows.NewLazySystemDLL("kernel32.dll").NewProc("GetConsoleScreenBufferInfo").Call(
		uintptr(handle), uintptr(unsafe.Pointer(&info)),
	)
	if r == 0 {
		return 0, 0, err
	}
	return int(info.srWindow.Right - info.srWindow.Left + 1),
		int(info.srWindow.Bottom - info.srWindow.Top + 1), nil
}
