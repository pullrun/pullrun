package cmd

import (
	"context"
	"fmt"
	"time"

	runtimepb "nimbus/protoapi/nimbus/runtime"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

// GRPCClient wraps a real gRPC connection to the nimbus-runtime service.
type GRPCClient struct {
	conn   *grpc.ClientConn
	client runtimepb.RuntimeClient
}

// NewGRPCClient dials a UDS socket and returns a client wrapper.
// The connection uses insecure transport (UDS is local-only).
func NewGRPCClient(socketPath string) (*GRPCClient, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, err := grpc.DialContext(
		ctx,
		"unix://"+socketPath,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithBlock(),
		grpc.WithDefaultCallOptions(grpc.WaitForReady(false)),
	)
	if err != nil {
		return nil, fmt.Errorf("dial %s: %w", socketPath, err)
	}

	return &GRPCClient{
		conn:   conn,
		client: runtimepb.NewRuntimeClient(conn),
	}, nil
}

// Close releases the gRPC connection.
func (c *GRPCClient) Close() error {
	if c.conn != nil {
		return c.conn.Close()
	}
	return nil
}

// PullImage fetches an OCI image and stores it in the local DAG.
func (c *GRPCClient) PullImage(ctx context.Context, imageRef, registry string) (*runtimepb.PullImageResponse, error) {
	return c.client.PullImage(ctx, &runtimepb.PullImageRequest{
		ImageRef: imageRef,
		Registry: registry,
	})
}

// RunWorkload executes a workload from a DAG root digest.
func (c *GRPCClient) RunWorkload(ctx context.Context, req *runtimepb.RunRequest) (*runtimepb.RunResponse, error) {
	return c.client.RunWorkload(ctx, req)
}

// StopWorkload terminates a running workload.
func (c *GRPCClient) StopWorkload(ctx context.Context, id string) (*runtimepb.StopResponse, error) {
	return c.client.StopWorkload(ctx, &runtimepb.StopRequest{Id: id})
}

// GetWorkload returns the current status of a workload.
func (c *GRPCClient) GetWorkload(ctx context.Context, id string) (*runtimepb.WorkloadStatus, error) {
	return c.client.GetWorkload(ctx, &runtimepb.GetWorkloadRequest{Id: id})
}

// ListWorkloads enumerates all known workloads.
func (c *GRPCClient) ListWorkloads(ctx context.Context) (*runtimepb.ListWorkloadsResponse, error) {
	return c.client.ListWorkloads(ctx, &runtimepb.ListWorkloadsRequest{})
}

// StreamLogs returns a server stream of log chunks.
func (c *GRPCClient) StreamLogs(ctx context.Context, id string, follow bool, tail int64) (runtimepb.Runtime_StreamLogsClient, error) {
	return c.client.StreamLogs(ctx, &runtimepb.StreamLogsRequest{
		Id:     id,
		Follow: follow,
		Tail:   tail,
	})
}

// StreamEvents returns a server stream of runtime events.
func (c *GRPCClient) StreamEvents(ctx context.Context, eventTypes []string) (runtimepb.Runtime_StreamEventsClient, error) {
	return c.client.StreamEvents(ctx, &runtimepb.StreamEventsRequest{
		EventTypes: eventTypes,
	})
}

// ExecInWorkload runs a command inside a running workload.
func (c *GRPCClient) ExecInWorkload(ctx context.Context, id string, command []string) (*runtimepb.ExecResponse, error) {
	return c.client.ExecInWorkload(ctx, &runtimepb.ExecRequest{
		Id:      id,
		Command: command,
	})
}

// InspectWorkload returns a deep snapshot of a workload: state,
// backend, image root, network rules, DAG path from manifest down
// to leaf blobs, and the policy decision log.
func (c *GRPCClient) InspectWorkload(ctx context.Context, id string) (*runtimepb.InspectResponse, error) {
	return c.client.InspectWorkload(ctx, &runtimepb.InspectRequest{Id: id})
}

// AttachWorkload opens a bidirectional I/O stream to a running
// workload. The returned stream is used by `nimbusctl workload
// run` to proxy the user's terminal to the workload's stdio.
//
// Callers MUST send an AttachMessage_Open as the first message;
// the runtime service rejects the stream otherwise.
func (c *GRPCClient) AttachWorkload(ctx context.Context) (runtimepb.Runtime_AttachWorkloadClient, error) {
	return c.client.AttachWorkload(ctx)
}
