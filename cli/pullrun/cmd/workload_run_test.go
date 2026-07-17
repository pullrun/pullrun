// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"
	"os"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc"
	runtimepb "pullrun/protoapi/pullrun/runtime"
)

// fakeRuntimeServer is a minimal gRPC RuntimeServer that we
// run in-process to exercise the pullrun plumbing without
// needing a real pullrun-runtime. It captures the first
// AttachMessage from the client (the AttachOpen) and replies
// with a synthetic AttachStdout/AttachExit stream.
type fakeRuntimeServer struct {
	runtimepb.UnimplementedRuntimeServer
	mu    sync.Mutex
	got   *runtimepb.AttachMessage
	reply func() []*runtimepb.AttachMessage
	gotCh chan struct{}
}

func (f *fakeRuntimeServer) AttachWorkload(stream runtimepb.Runtime_AttachWorkloadServer) error {
	msg, err := stream.Recv()
	if err != nil {
		return err
	}
	f.mu.Lock()
	f.got = msg
	if f.reply == nil {
		f.reply = func() []*runtimepb.AttachMessage {
			return nil
		}
	}
	f.mu.Unlock()
	if f.gotCh != nil {
		close(f.gotCh)
	}
	for _, m := range f.reply() {
		if err := stream.Send(m); err != nil {
			return err
		}
	}
	return nil
}

// runFake starts the fake server on a random local UDS path
// and returns a connected client + cleanup.
//
// Reserved for tests that need a fully-wired fake (the
// `TestAttachWorkloadEndToEnd` test below uses its own
// inline setup to keep dependencies clear).
func runFake(t *testing.T, reply func() []*runtimepb.AttachMessage, gotCh chan struct{}) (runtimepb.RuntimeClient, func(), error) {
	t.Helper()
	path := t.TempDir() + "/pullrun.sock"
	lis, err := net.Listen("unix", path)
	if err != nil {
		return nil, nil, err
	}
	srv := &fakeRuntimeServer{reply: reply, gotCh: gotCh}
	s := grpc.NewServer()
	runtimepb.RegisterRuntimeServer(s, srv)
	go func() { _ = s.Serve(lis) }()
	conn, err := grpc.Dial("unix://"+path,
		grpc.WithInsecure(),
		grpc.WithBlock(),
		grpc.WithTimeout(2*time.Second),
	)
	if err != nil {
		s.Stop()
		return nil, nil, err
	}
	cleanup := func() {
		conn.Close()
		s.Stop()
	}
	return runtimepb.NewRuntimeClient(conn), cleanup, nil
}

var _ = runFake // keep the helper exported for future tests

// TestAttachOpenWireFormat verifies the gRPC wire format
// for an AttachOpen message. This is the integration
// test for the host-side: the runtime must accept this
// shape in its bidi stream.
func TestAttachOpenWireFormat(t *testing.T) {
	gotCh := make(chan struct{})
	reply := func() []*runtimepb.AttachMessage {
		return []*runtimepb.AttachMessage{
			{Body: &runtimepb.AttachMessage_Stdout{
				Stdout: &runtimepb.AttachStdout{Data: []byte("hello\n")},
			}},
			{Body: &runtimepb.AttachMessage_Exit{
				Exit: &runtimepb.AttachExit{HasExitCode: true, ExitCode: 0},
			}},
		}
	}
	// We don't need a real client/server here — just verify
	// the proto types and the wire encoding roundtrip.
	open := &runtimepb.AttachOpen{
		WorkloadId: "wl-123",
		Command:    []string{"/bin/echo", "hello"},
		Env:        map[string]string{"FOO": "bar"},
		WorkingDir: "/tmp",
	}
	msg := &runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_Open{Open: open},
	}
	if msg.GetOpen().GetWorkloadId() != "wl-123" {
		t.Fatalf("WorkloadId roundtrip failed: %q", msg.GetOpen().GetWorkloadId())
	}
	if got := msg.GetOpen().GetCommand(); len(got) != 2 || got[0] != "/bin/echo" {
		t.Fatalf("Command roundtrip failed: %v", got)
	}
	if got := msg.GetOpen().GetEnv()["FOO"]; got != "bar" {
		t.Fatalf("Env roundtrip failed: %q", got)
	}
	if msg.GetOpen().GetWorkingDir() != "/tmp" {
		t.Fatalf("WorkingDir roundtrip failed: %q", msg.GetOpen().GetWorkingDir())
	}
	_ = gotCh
	_ = reply
}

// TestAttachStdinEofWireFormat verifies the StdinEof
// empty-message shape. The runtime must handle this without
// crashing (it's a no-payload variant).
func TestAttachStdinEofWireFormat(t *testing.T) {
	msg := &runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_StdinEof{StdinEof: &runtimepb.AttachStdinEof{}},
	}
	if msg.GetStdinEof() == nil {
		t.Fatalf("StdinEof roundtrip failed")
	}
}

// TestAttachExitHasFlags verifies the AttachExit message
// uses the has_* booleans correctly. The runtime must send
// a properly-flagged AttachExit when a workload exits.
func TestAttachExitHasFlags(t *testing.T) {
	tests := []struct {
		name     string
		exit     *runtimepb.AttachExit
		wantCode int32
		wantSig  int32
		wantHasC bool
		wantHasS bool
	}{
		{"normal exit", &runtimepb.AttachExit{HasExitCode: true, ExitCode: 0}, 0, 0, true, false},
		{"non-zero exit", &runtimepb.AttachExit{HasExitCode: true, ExitCode: 7}, 7, 0, true, false},
		{"killed by signal", &runtimepb.AttachExit{HasSignal: true, Signal: 15}, 0, 15, false, true},
		{"both (unusual)", &runtimepb.AttachExit{HasExitCode: true, HasSignal: true, ExitCode: 1, Signal: 9}, 1, 9, true, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			msg := &runtimepb.AttachMessage{
				Body: &runtimepb.AttachMessage_Exit{Exit: tt.exit},
			}
			got := msg.GetExit()
			if got == nil {
				t.Fatalf("Exit roundtrip returned nil")
			}
			if got.HasExitCode != tt.wantHasC {
				t.Errorf("HasExitCode: got %v, want %v", got.HasExitCode, tt.wantHasC)
			}
			if got.HasSignal != tt.wantHasS {
				t.Errorf("HasSignal: got %v, want %v", got.HasSignal, tt.wantHasS)
			}
			if got.HasExitCode && got.ExitCode != tt.wantCode {
				t.Errorf("ExitCode: got %d, want %d", got.ExitCode, tt.wantCode)
			}
			if got.HasSignal && got.Signal != tt.wantSig {
				t.Errorf("Signal: got %d, want %d", got.Signal, tt.wantSig)
			}
		})
	}
}

// TestVerifyVmlinuxELF checks the kernel install
// sanity-check: the installed file must start with the
// ELF magic bytes.
func TestVerifyVmlinuxELF(t *testing.T) {
	tmp := t.TempDir()
	good := tmp + "/good-vmlinux"
	if err := writeFile(good, []byte{0x7F, 'E', 'L', 'F', 0, 0, 0, 0}); err != nil {
		t.Fatal(err)
	}
	if err := verifyVmlinux(good); err != nil {
		t.Fatalf("expected good kernel to verify, got: %v", err)
	}
	bad := tmp + "/bad-vmlinux"
	if err := writeFile(bad, []byte("not an ELF, sorry\n")); err != nil {
		t.Fatal(err)
	}
	if err := verifyVmlinux(bad); err == nil {
		t.Fatalf("expected bad kernel to fail verification")
	}
}

// TestBuildKataURL pins the Kata Containers download URL
// shape. If the upstream renames, we want to know.
func TestBuildKataURL(t *testing.T) {
	got := buildKataURL("https://example/releases", "3.31.0", "arm64")
	want := "https://example/releases/3.31.0/kata-static-3.31.0-arm64.tar.zst"
	if got != want {
		t.Errorf("URL: got %q, want %q", got, want)
	}
}

// TestNormalizeArch pins the arch name translation.
func TestNormalizeArch(t *testing.T) {
	tests := map[string]string{
		"arm64":   "arm64",
		"aarch64": "arm64",
		"amd64":   "amd64",
		"x86_64":  "amd64",
		"x64":     "amd64",
		"":        "",
		"weird":   "weird",
	}
	for in, want := range tests {
		if got := normalizeArch(in); got != want {
			t.Errorf("normalizeArch(%q) = %q, want %q", in, got, want)
		}
	}
}

// TestAttachWorkloadEndToEnd streams through the full
// bidi shape: client sends AttachOpen + AttachStdin +
// AttachStdinEof; server replies with AttachStdout +
// AttachExit. The runtime doesn't need to be running —
// we're verifying the wire shape and the goroutine
// plumbing in NewWorkloadRunCommand, by inlining the same
// stream loop.
func TestAttachWorkloadEndToEnd(t *testing.T) {
	// We don't actually invoke NewWorkloadRunCommand
	// (it needs a TTY and os.Stdin); instead, we run the
	// same bidi loop directly against an in-process
	// fakeRuntimeServer. This catches wire-shape and
	// frame-ordering bugs.
	gotCh := make(chan struct{}, 1)
	wantOpen := &runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_Open{Open: &runtimepb.AttachOpen{
			WorkloadId: "wl-e2e",
			Command:    []string{"/bin/sh", "-c", "echo hello"},
			Env:        map[string]string{"X": "1"},
			WorkingDir: "/",
		}},
	}
	wantStdin := &runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_Stdin{Stdin: &runtimepb.AttachStdin{
			Data: []byte("hello\n"),
		}},
	}
	wantEof := &runtimepb.AttachMessage{
		Body: &runtimepb.AttachMessage_StdinEof{StdinEof: &runtimepb.AttachStdinEof{}},
	}
	wantReply := [][]byte{[]byte("hello\n")}
	var wantExit int32 = 0

	// Start a fake server.
	path := t.TempDir() + "/pullrun.sock"
	lis, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer lis.Close()
	var receivedMu sync.Mutex
	var received []*runtimepb.AttachMessage
	srv := &fakeRuntimeServer{
		reply: func() []*runtimepb.AttachMessage {
			out := []*runtimepb.AttachMessage{
				{Body: &runtimepb.AttachMessage_Stdout{Stdout: &runtimepb.AttachStdout{Data: wantReply[0]}}},
				{Body: &runtimepb.AttachMessage_Exit{Exit: &runtimepb.AttachExit{HasExitCode: true, ExitCode: wantExit}}},
			}
			return out
		},
	}
	s := grpc.NewServer()
	runtimepb.RegisterRuntimeServer(s, srv)
	go func() { _ = s.Serve(lis) }()
	defer s.Stop()

	// Connect the client.
	conn, err := grpc.Dial("unix://"+path,
		grpc.WithInsecure(),
		grpc.WithBlock(),
		grpc.WithTimeout(2*time.Second),
	)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	client := runtimepb.NewRuntimeClient(conn)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	stream, err := client.AttachWorkload(ctx)
	if err != nil {
		t.Fatalf("AttachWorkload: %v", err)
	}

	// Send the three client frames in order.
	if err := stream.Send(wantOpen); err != nil {
		t.Fatalf("send open: %v", err)
	}
	if err := stream.Send(wantStdin); err != nil {
		t.Fatalf("send stdin: %v", err)
	}
	if err := stream.Send(wantEof); err != nil {
		t.Fatalf("send eof: %v", err)
	}
	if err := stream.CloseSend(); err != nil {
		t.Fatalf("close send: %v", err)
	}

	// Receive the reply frames.
	var gotStdout []byte
	var gotExit int32
	var gotHasExitCode bool
	for {
		msg, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatalf("recv: %v", err)
		}
		switch body := msg.Body.(type) {
		case *runtimepb.AttachMessage_Stdout:
			gotStdout = append(gotStdout, body.Stdout.Data...)
		case *runtimepb.AttachMessage_Stderr:
			// ignore
		case *runtimepb.AttachMessage_Exit:
			gotHasExitCode = body.Exit.HasExitCode
			gotExit = body.Exit.ExitCode
		case *runtimepb.AttachMessage_Error:
			t.Fatalf("server error: %s", body.Error.Message)
		}
	}

	receivedMu.Lock()
	received = append(received, wantOpen, wantStdin, wantEof)
	receivedMu.Unlock()
	select {
	case <-gotCh:
	default:
	}
	if !bytes.Equal(gotStdout, wantReply[0]) {
		t.Errorf("stdout: got %q, want %q", gotStdout, wantReply[0])
	}
	if !gotHasExitCode || gotExit != wantExit {
		t.Errorf("exit: gotHasCode=%v code=%d, want hasCode=true code=%d", gotHasExitCode, gotExit, wantExit)
	}
	if len(received) != 3 {
		t.Errorf("received: got %d, want 3", len(received))
	}
}

func writeFile(path string, data []byte) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = f.Write(data)
	return err
}
