package main

import (
	"context"
	"fmt"
	"log"
	"net"
	"net/http"
	"path/filepath"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	pb "pullrun/protoapi/pullrun/control"
)

type WorkloadRecord struct {
	ID         string
	Name       string
	ImageRef   string
	Backend    string
	CPUMillis  uint64
	MemoryB    uint64
	Labels     map[string]string
	CreatedAt  time.Time
	NodeID     string
	Status     string
}

type NodeRecord struct {
	ID                string
	Address           string
	CPUCores          uint64
	MemoryBytes       uint64
	AvailableBackends []string
	LastHeartbeat     time.Time
	RunningCount      uint64
}

func main() {
	listen := ":8080"
	storeRoot := "/var/lib/pullrun/control-plane"

	// Create file-backed store (survives restarts, no etcd needed for v0).
	store := newFileStore(storeRoot)

	// Try to connect to the local runtime for direct dispatch
	runtimeSock := filepath.Join(filepath.Dir(storeRoot), "runtime.sock")
	conn, err := grpc.NewClient(
		"unix://"+runtimeSock,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err == nil {
		store.runtimeConn = conn
		log.Printf("control plane connected to runtime at %s", runtimeSock)
	} else {
		log.Printf("warning: %v (control plane runs in passive mode)", err)
	}

	// Start gRPC server
	go func() {
		lis, err := net.Listen("tcp", listen)
		if err != nil {
			log.Fatalf("failed to listen: %v", err)
		}
		grpcServer := grpc.NewServer()
		pb.RegisterControlPlaneServer(grpcServer, store)
		log.Printf("control plane gRPC listening on %s", listen)
		if err := grpcServer.Serve(lis); err != nil {
			log.Fatalf("gRPC server error: %v", err)
		}
	}()

	// Start HTTP server for healthz and simple REST API
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("/api/workloads", func(w http.ResponseWriter, r *http.Request) {
		wls, _ := store.ListWorkloads(r.Context(), &pb.ListRequest{})
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "[")
		for i, wl := range wls.Items {
			if i > 0 {
				fmt.Fprintf(w, ",")
			}
			fmt.Fprintf(w, `{"id":"%s","state":"%s","backend":"%s"}`,
				wl.Id, wl.State, wl.Backend)
		}
		fmt.Fprintf(w, "]")
	})
	mux.HandleFunc("/api/nodes", func(w http.ResponseWriter, r *http.Request) {
		nodes := store.ListNodes()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "[")
		first := true
		for _, n := range nodes {
			if !first {
				fmt.Fprintf(w, ",")
			}
			first = false
			fmt.Fprintf(w, `{"id":"%s","address":"%s","backends":%d}`,
				n.ID, n.Address, len(n.AvailableBackends))
		}
		fmt.Fprintf(w, "]")
	})

	log.Printf("control plane HTTP listening on :8081")
	srv := &http.Server{
		Addr:    ":8081",
		Handler: mux,
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Printf("HTTP server error: %v", err)
		}
	}()

	<-ctx.Done()
	log.Println("shutting down")
}
