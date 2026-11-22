// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"fmt"
	"log"
	"strings"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"time"

	runtimeapi "k8s.io/cri-api/pkg/apis/runtime/v1"
	pullrunruntime "pullrun/protoapi/pullrun/runtime"
)

// ============================================================
// Image service — translates CRI image ops to pullrun-runtime PullImage /
// ListWorkloads. In v0 images are content-addressed by their DAG root
// digest; CRI's image id == pullrun root digest.
// ============================================================

func (c *criServer) PullImage(ctx context.Context, req *runtimeapi.PullImageRequest) (*runtimeapi.PullImageResponse, error) {
	if req.Image == nil {
		return nil, fmt.Errorf("PullImage: missing image spec")
	}

	log.Printf("PullImage image=%s", req.Image.Image)

	pullCtx, cancel := context.WithTimeout(ctx, 10*60*1e9)
	// Pass an empty Registry so the pullrun-runtime uses its default
	// (registry-1.docker.io for Docker Hub). CRI doesn't tell us the
	// registry directly, and runtime's puller expects real registry
	// hostnames (not aliases like "docker.io").
	resp, err := c.runtimeClient.PullImage(pullCtx, &pullrunruntime.PullImageRequest{
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

	hasCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	resp, err := c.runtimeClient.HasImage(hasCtx, &pullrunruntime.HasImageRequest{
		ImageRef: req.Image.Image,
	})
	cancel()

	if err != nil {
		log.Printf("ImageStatus image=%s (runtime error: %v)", req.Image.Image, err)
		return &runtimeapi.ImageStatusResponse{}, nil
	}

	if !resp.Exists {
		log.Printf("ImageStatus image=%s (not found)", req.Image.Image)
		return &runtimeapi.ImageStatusResponse{}, nil
	}

	ref := req.Image.Image
	if !strings.Contains(ref, "@") {
		last := ref[strings.LastIndex(ref, "/")+1:]
		if !strings.Contains(last, ":") {
			ref += ":latest"
		}
	}
	uid := &runtimeapi.Int64Value{Value: 0}
	return &runtimeapi.ImageStatusResponse{
		Image: &runtimeapi.Image{
			Id:       resp.RootDigest,
			RepoTags: []string{ref},
			Size:     uint64(resp.SizeBytes),
			Uid:      uid,
			Username: "root",
			Spec:     &runtimeapi.ImageSpec{Image: req.Image.Image},
		},
	}, nil
}

func (c *criServer) ListImages(ctx context.Context, req *runtimeapi.ListImagesRequest) (*runtimeapi.ListImagesResponse, error) {
	listCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	resp, err := c.runtimeClient.ListImages(listCtx, &pullrunruntime.ListImagesRequest{})
	cancel()

	if err != nil {
		log.Printf("ListImages (runtime error: %v)", err)
		return &runtimeapi.ListImagesResponse{}, nil
	}

	byID := make(map[string]*runtimeapi.Image)
	order := make([]string, 0, len(resp.Images))
	for _, img := range resp.Images {
		var size uint64
		if img.SizeBytes > 0 {
			size = uint64(img.SizeBytes)
		}
		if existing, ok := byID[img.RootDigest]; ok {
			existing.RepoTags = append(existing.RepoTags, img.ImageRef)
			continue
		}
		uid := &runtimeapi.Int64Value{Value: 0}
		byID[img.RootDigest] = &runtimeapi.Image{
			Id:       img.RootDigest,
			RepoTags: []string{img.ImageRef},
			Size:     size,
			Uid:      uid,
			Username: "root",
			Spec:     &runtimeapi.ImageSpec{Image: img.ImageRef},
		}
		order = append(order, img.RootDigest)
	}
	images := make([]*runtimeapi.Image, 0, len(byID))
	for _, id := range order {
		images = append(images, byID[id])
	}
	return &runtimeapi.ListImagesResponse{Images: images}, nil
}

func (c *criServer) RemoveImage(ctx context.Context, req *runtimeapi.RemoveImageRequest) (*runtimeapi.RemoveImageResponse, error) {
	// Resolve tag/name references to the runtime's root digest.
	hasCtx, hasCancel := context.WithTimeout(ctx, 5*1e9)
	has, hasErr := c.runtimeClient.HasImage(hasCtx, &pullrunruntime.HasImageRequest{ImageRef: req.Image.Image})
	hasCancel()
	root := req.Image.Image
	if hasErr == nil && has.Exists && has.RootDigest != "" {
		root = has.RootDigest
	}

	removeCtx, cancel := context.WithTimeout(ctx, 60*1e9)
	resp, err := c.runtimeClient.RemoveImage(removeCtx, &pullrunruntime.RemoveImageRequest{
		RootDigest: root,
	})
	cancel()

	if err != nil {
		if status.Code(err) == codes.NotFound {
			return &runtimeapi.RemoveImageResponse{}, nil
		}
		log.Printf("RemoveImage image=%s (runtime error: %v)", req.Image.Image, err)
		return nil, fmt.Errorf("remove %s: %w", req.Image.Image, err)
	}

	log.Printf("RemoveImage image=%s freed=%d bytes", req.Image.Image, resp.BytesFreed)
	return &runtimeapi.RemoveImageResponse{}, nil
}

func (c *criServer) ImageFsInfo(ctx context.Context, req *runtimeapi.ImageFsInfoRequest) (*runtimeapi.ImageFsInfoResponse, error) {
	infoCtx, cancel := context.WithTimeout(ctx, 5*1e9)
	resp, err := c.runtimeClient.DagStoreInfo(infoCtx, &pullrunruntime.DagStoreInfoRequest{})
	cancel()

	var mountpoint string
	var usedBytes uint64
	var totalNodes uint64
	if err != nil {
		log.Printf("ImageFsInfo (runtime error: %v)", err)
		mountpoint = "/var/lib/pullrun/dag"
	} else {
		log.Printf("ImageFsInfo mount=%s total=%d used=%d nodes=%d",
			resp.Mountpoint, resp.TotalBytes, resp.UsedBytes, resp.TotalNodes)
		mountpoint = resp.Mountpoint
		if resp.UsedBytes > 0 {
			usedBytes = uint64(resp.UsedBytes)
		}
		if resp.TotalNodes > 0 {
			totalNodes = uint64(resp.TotalNodes)
		}
	}

	return &runtimeapi.ImageFsInfoResponse{
		ImageFilesystems: []*runtimeapi.FilesystemUsage{
			{
				Timestamp:  time.Now().UnixNano(),
				FsId:       &runtimeapi.FilesystemIdentifier{Mountpoint: mountpoint},
				UsedBytes:  &runtimeapi.UInt64Value{Value: usedBytes},
				InodesUsed: &runtimeapi.UInt64Value{Value: totalNodes},
			},
		},
	}, nil
}
