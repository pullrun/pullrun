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
			io.Copy(stream, tcpConn)
			tcpConn.Close()
		}()
		go func() {
			defer wg.Done()
			io.Copy(tcpConn, stream)
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

	var grpcWg sync.WaitGroup
	grpcWg.Add(2)

	go func() {
		defer grpcWg.Done()
		for msg := range stdinCh {
			if err := grpcStream.Send(msg); err != nil {
				log.Printf("grpc send: %v", err)
				return
			}
		}
	}()

	go func() {
		defer grpcWg.Done()
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
				return
			case *pullrunruntime.AttachMessage_Error:
				errCh <- fmt.Errorf("runtime error: %s", b.Error.Message)
				return
			}
		}
	}()

	upgrader := spdy.NewResponseUpgrader()
	var spdyWg sync.WaitGroup

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
		case streamTypeStdin:
			spdyWg.Add(1)
			go func() {
				defer spdyWg.Done()
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
						default:
							log.Printf("stdin channel full, dropping data")
						}
					}
					if err != nil {
						if err == io.EOF {
							stdinCh <- &pullrunruntime.AttachMessage{
								Body: &pullrunruntime.AttachMessage_StdinEof{
									StdinEof: &pullrunruntime.AttachStdinEof{},
								},
							}
						}
						return
					}
				}
			}()

		case streamTypeStdout:
			spdyWg.Add(1)
			go func() {
				defer spdyWg.Done()
				defer stream.Close()
				for data := range stdoutCh {
					if _, err := stream.Write(data); err != nil {
						return
					}
				}
			}()

		case streamTypeStderr:
			spdyWg.Add(1)
			go func() {
				defer spdyWg.Done()
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

	spdyWg.Wait()
	close(stdinCh)
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
