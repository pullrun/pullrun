// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"

	"k8s.io/streaming/pkg/httpstream"
	"k8s.io/streaming/pkg/httpstream/spdy"

	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

const (
	streamTypeStdin  = "stdin"
	streamTypeStdout = "stdout"
	streamTypeStderr = "stderr"
	streamTypeResize = "resize"
	streamTypeError  = "error"

	streamProtocolV4 = "v4.channel.k8s.io"
)

type execSession struct {
	workloadID string
	command    []string
	env        map[string]string
	workingDir string
	attach     bool
	createdAt  time.Time
}

type portForwardSession struct {
	workloadID string
	targetIP   string
	port       int32
	createdAt  time.Time
}

type streamingServer struct {
	port          int
	server        *http.Server
	runtimeClient pullrunruntime.RuntimeClient
	sessions      sync.Map
}

// writeExitStatus reports the process exit code to the v4 remotecommand
// client via a Status JSON on the error stream, then closes it.
func writeExitStatus(stream httpstream.Stream, exitCode int) {
	if stream == nil {
		return
	}
	var status string
	if exitCode == 0 {
		status = `{"status":"Success"}`
	} else {
		status = fmt.Sprintf(
			`{"status":"Failure","reason":"NonZeroExitCode","details":{"causes":[{"reason":"ExitCode","message":"%d"}]}}`,
			exitCode,
		)
	}
	io.WriteString(stream, status)
	stream.Close()
}

// writeErrorStatus reports a runtime error to the v4 remotecommand client.
func writeErrorStatus(stream httpstream.Stream, message string) {
	if stream == nil {
		return
	}
	io.WriteString(stream, fmt.Sprintf(`{"status":"Failure","reason":"RuntimeError","message":%q}`, message))
	stream.Close()
}

func newStreamingServer(runtimeClient pullrunruntime.RuntimeClient) (*streamingServer, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("streaming listen: %w", err)
	}

	s := &streamingServer{
		port:          listener.Addr().(*net.TCPAddr).Port,
		runtimeClient: runtimeClient,
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/exec/", s.serveAttach)
	mux.HandleFunc("/attach/", s.serveAttach)
	mux.HandleFunc("/port-forward/", s.servePortForward)

	s.server = &http.Server{Handler: mux}

	go func() {
		if err := s.server.Serve(listener); err != nil && err != http.ErrServerClosed {
			log.Printf("streaming server error: %v", err)
		}
	}()

	log.Printf("streaming server on 127.0.0.1:%d", s.port)
	return s, nil
}

func (s *streamingServer) newSession(workloadID string, command []string, env map[string]string, workingDir string, attach bool) (string, string) {
	token := generateToken()
	s.sessions.Store(token, &execSession{
		workloadID: workloadID,
		command:    command,
		env:        env,
		workingDir: workingDir,
		attach:     attach,
		createdAt:  time.Now(),
	})

	time.AfterFunc(5*time.Minute, func() {
		s.sessions.Delete(token)
	})

	path := "exec"
	if attach {
		path = "attach"
	}
	return token, fmt.Sprintf("http://127.0.0.1:%d/%s/%s", s.port, path, token)
}

func (s *streamingServer) newPortForwardSession(workloadID string, targetIP string, port int32) (string, string) {
	token := generateToken()
	s.sessions.Store(token, &portForwardSession{
		workloadID: workloadID,
		targetIP:   targetIP,
		port:       port,
		createdAt:  time.Now(),
	})
	time.AfterFunc(5*time.Minute, func() {
		s.sessions.Delete(token)
	})
	return token, fmt.Sprintf("http://127.0.0.1:%d/port-forward/%s", s.port, token)
}

func (s *streamingServer) servePortForward(w http.ResponseWriter, r *http.Request) {
	token := extractToken(r.URL.Path)
	if token == "" {
		http.Error(w, "missing token", http.StatusBadRequest)
		return
	}

	raw, ok := s.sessions.Load(token)
	if !ok {
		http.Error(w, "session not found or expired", http.StatusNotFound)
		return
	}
	session := raw.(*portForwardSession)
	defer s.sessions.Delete(token)

	upgrader := spdy.NewResponseUpgrader()
	conn := upgrader.UpgradeResponse(w, r, func(stream httpstream.Stream, _ <-chan struct{}) error {
		portStr := stream.Headers().Get("port")
		if portStr == "" {
			portStr = fmt.Sprintf("%d", session.port)
		}
		targetAddr := net.JoinHostPort(session.targetIP, portStr)
		tcpConn, err := net.DialTimeout("tcp", targetAddr, 5*time.Second)
		if err != nil {
			log.Printf("port-forward dial %s: %v", targetAddr, err)
			return err
		}

		var wg sync.WaitGroup
		wg.Add(2)
		go func() {
			defer wg.Done()
			_, _ = io.Copy(stream, tcpConn)
			tcpConn.Close()
		}()
		go func() {
			defer wg.Done()
			_, _ = io.Copy(tcpConn, stream)
			stream.Close()
		}()
		wg.Wait()
		return nil
	})
	if conn == nil {
		log.Printf("port-forward: spdy upgrade returned nil connection")
	}
}

func (s *streamingServer) serveAttach(w http.ResponseWriter, r *http.Request) {
	token := extractToken(r.URL.Path)
	if token == "" {
		http.Error(w, "missing token", http.StatusBadRequest)
		return
	}

	raw, ok := s.sessions.Load(token)
	if !ok {
		http.Error(w, "session not found or expired", http.StatusNotFound)
		return
	}
	session := raw.(*execSession)
	defer s.sessions.Delete(token)

	grpcStream, err := s.runtimeClient.AttachWorkload(r.Context())
	if err != nil {
		log.Printf("attach stream error: %v", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	if err := grpcStream.Send(&pullrunruntime.AttachMessage{
		Body: &pullrunruntime.AttachMessage_Open{
			Open: &pullrunruntime.AttachOpen{
				WorkloadId: session.workloadID,
				Command:    session.command,
				Env:        session.env,
				WorkingDir: session.workingDir,
			},
		},
	}); err != nil {
		log.Printf("attach open send: %v", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	stdinCh := make(chan *pullrunruntime.AttachMessage, 256)
	stdoutCh := make(chan []byte, 256)
	stderrCh := make(chan []byte, 256)
	exitCh := make(chan *pullrunruntime.AttachExit, 1)
	errCh := make(chan error, 1)
	quit := make(chan struct{})
	sessionDone := make(chan struct{})

	var errorStream httpstream.Stream
	var streamMu sync.Mutex

	var grpcWg sync.WaitGroup
	grpcWg.Add(2)

	go func() {
		defer grpcWg.Done()
		for {
			select {
			case msg, ok := <-stdinCh:
				if !ok {
					return
				}
				if err := grpcStream.Send(msg); err != nil {
					log.Printf("grpc send: %v", err)
					return
				}
			case <-quit:
				return
			}
		}
	}()

	go func() {
		defer grpcWg.Done()
		defer close(sessionDone)
		defer close(stdoutCh)
		defer close(stderrCh)
		defer close(exitCh)
		defer close(errCh)

		for {
			msg, err := grpcStream.Recv()
			if err != nil {
				if err != io.EOF {
					errCh <- err
				}
				return
			}
			switch b := msg.Body.(type) {
			case *pullrunruntime.AttachMessage_Stdout:
				select {
				case stdoutCh <- b.Stdout.Data:
				default:
				}
			case *pullrunruntime.AttachMessage_Stderr:
				select {
				case stderrCh <- b.Stderr.Data:
				default:
				}
			case *pullrunruntime.AttachMessage_Exit:
				exitCh <- b.Exit
				streamMu.Lock()
				es := errorStream
				streamMu.Unlock()
				writeExitStatus(es, int(b.Exit.ExitCode))
				return
			case *pullrunruntime.AttachMessage_Error:
				errCh <- fmt.Errorf("runtime error: %s", b.Error.Message)
				streamMu.Lock()
				es := errorStream
				streamMu.Unlock()
				writeErrorStatus(es, b.Error.Message)
				return
			}
		}
	}()

	upgrader := spdy.NewResponseUpgrader()
	if _, err := httpstream.Handshake(r, w, []string{streamProtocolV4}); err != nil {
		log.Printf("exec protocol handshake: %v", err)
		return
	}

	// Drain exit/error channels so the gRPC receiver goroutine
	// never blocks indefinitely. Log any errors received.
	go func() {
		select {
		case exit := <-exitCh:
			log.Printf("workload %s exited: code=%d", session.workloadID, exit.ExitCode)
		case err := <-errCh:
			log.Printf("workload %s stream error: %v", session.workloadID, err)
		}
	}()

	conn := upgrader.UpgradeResponse(w, r, func(stream httpstream.Stream, _ <-chan struct{}) error {
		st := stream.Headers().Get("streamType")
		switch st {
		case streamTypeError:
			streamMu.Lock()
			errorStream = stream
			streamMu.Unlock()
			return nil
		case streamTypeStdin:
			go func() {
				defer stream.Close()
				buf := make([]byte, 32768)
				for {
					n, err := stream.Read(buf)
					if n > 0 {
						data := make([]byte, n)
						copy(data, buf[:n])
						select {
						case stdinCh <- &pullrunruntime.AttachMessage{
							Body: &pullrunruntime.AttachMessage_Stdin{
								Stdin: &pullrunruntime.AttachStdin{Data: data},
							},
						}:
						case <-quit:
							return
						}
					}
					if err != nil {
						if err == io.EOF {
							select {
							case stdinCh <- &pullrunruntime.AttachMessage{
								Body: &pullrunruntime.AttachMessage_StdinEof{
									StdinEof: &pullrunruntime.AttachStdinEof{},
								},
							}:
							case <-quit:
							}
						} else {
							log.Printf("stdin: read error: %v", err)
						}
						return
					}
				}
			}()

		case streamTypeStdout:
			go func() {
				defer stream.Close()
				for data := range stdoutCh {
					if _, err := stream.Write(data); err != nil {
						return
					}
				}
			}()

		case streamTypeStderr:
			go func() {
				defer stream.Close()
				for data := range stderrCh {
					if _, err := stream.Write(data); err != nil {
						return
					}
				}
			}()

		case streamTypeResize:
			defer stream.Close()
		}
		return nil
	})

	if conn == nil {
		log.Printf("spdy upgrade returned nil connection")
		return
	}

	// Wait for the runtime session to end (Exit/Error/stream EOF),
	// then tear down the connection: closing it wakes any stream
	// goroutine still blocked in Read/Write.
	<-sessionDone
	conn.Close()
	close(quit)
	grpcWg.Wait()
}

func extractToken(path string) string {
	path = strings.TrimSuffix(path, "/")
	parts := strings.Split(path, "/")
	if len(parts) < 3 {
		return ""
	}
	return parts[len(parts)-1]
}

func generateToken() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		panic(fmt.Sprintf("rand.Read: %v", err))
	}
	return hex.EncodeToString(b)
}
