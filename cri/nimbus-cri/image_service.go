package main

import (
	"context"
	"fmt"
	"log"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	nimbusruntime "nimbus/protoapi/nimbus/runtime"
)

// ============================================================
// Image service — translates CRI image ops to nimbus-runtime PullImage /
// ListWorkloads. In v0 images are content-addressed by their DAG root
// digest; CRI's image id == nimbus root digest.
// ============================================================

func (c *criServer) PullImage(ctx context.Context, req *runtimeapi.PullImageRequest) (*runtimeapi.PullImageResponse, error) {
	if req.Image == nil {
		return nil, fmt.Errorf("PullImage: missing image spec")
	}

	log.Printf("PullImage image=%s", req.Image.Image)

	pullCtx, cancel := context.WithTimeout(ctx, 10*60*1e9)
	// Pass an empty Registry so the nimbus-runtime uses its default
	// (registry-1.docker.io for Docker Hub). CRI doesn't tell us the
	// registry directly, and runtime's puller expects real registry
	// hostnames (not aliases like "docker.io").
	resp, err := c.runtimeClient.PullImage(pullCtx, &nimbusruntime.PullImageRequest{
		ImageRef: req.Image.Image,
		Registry: "",
	})
	cancel()
	if err != nil {
		return nil, fmt.Errorf("pull %s: %w", req.Image.Image, err)
	}

	return &runtimeapi.PullImageResponse{
		ImageRef: resp.RootDigest,
	}, nil
}

func (c *criServer) ImageStatus(ctx context.Context, req *runtimeapi.ImageStatusRequest) (*runtimeapi.ImageStatusResponse, error) {
	if req.Image == nil {
		return nil, fmt.Errorf("ImageStatus: missing image spec")
	}

	// v0: we don't maintain a separate image index. Anything that's been
	// pulled lives in the DAG store. We can probe the store by attempting
	// to look up the digest. For now, return unknown — kubelet will retry
	// with PullImage if needed.
	log.Printf("ImageStatus image=%s (v0: returns unknown)", req.Image.Image)
	return &runtimeapi.ImageStatusResponse{}, nil
}

func (c *criServer) ListImages(ctx context.Context, req *runtimeapi.ListImagesRequest) (*runtimeapi.ListImagesResponse, error) {
	// v0: we don't index images in the CRI shim. A real implementation would
	// walk the DAG store and return one entry per unique root digest.
	log.Printf("ListImages (v0: returns empty)")
	return &runtimeapi.ListImagesResponse{}, nil
}

func (c *criServer) RemoveImage(ctx context.Context, req *runtimeapi.RemoveImageRequest) (*runtimeapi.RemoveImageResponse, error) {
	// v0: images are content-addressed; we don't remove them (other workloads
	// may still reference them). A real implementation would mark the root
	// digest as garbage.
	log.Printf("RemoveImage image=%s (v0: no-op, content-addressed)", req.Image.Image)
	return &runtimeapi.RemoveImageResponse{}, nil
}

func (c *criServer) ImageFsInfo(ctx context.Context, req *runtimeapi.ImageFsInfoRequest) (*runtimeapi.ImageFsInfoResponse, error) {
	// v0: report the DAG store path as the image filesystem. A real
	// implementation would report overlayfs/extract paths used by the
	// runtime.
	log.Printf("ImageFsInfo (v0: stub)")
	return &runtimeapi.ImageFsInfoResponse{
		ImageFilesystems: []*runtimeapi.FilesystemUsage{
			{
				Timestamp:  time.Now().UnixNano(),
				FsId:       &runtimeapi.FilesystemIdentifier{Mountpoint: "/var/lib/nimbus/dag"},
				UsedBytes:  &runtimeapi.UInt64Value{Value: 0},
				InodesUsed: &runtimeapi.UInt64Value{Value: 0},
			},
		},
	}, nil
}
