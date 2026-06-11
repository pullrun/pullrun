module pullrun/cri

go 1.26.0

require (
	google.golang.org/grpc v1.64.0
	k8s.io/cri-api v0.30.0
	k8s.io/streaming v0.36.1
	pullrun/protoapi v0.0.0
)

require (
	github.com/go-logr/logr v1.4.3 // indirect
	github.com/gogo/protobuf v1.3.2 // indirect
	github.com/moby/spdystream v0.5.1 // indirect
	golang.org/x/net v0.49.0 // indirect
	golang.org/x/sys v0.40.0 // indirect
	golang.org/x/text v0.33.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20240318140521-94a12d6c2237 // indirect
	google.golang.org/protobuf v1.36.12-0.20260120151049-f2248ac996af // indirect
	k8s.io/klog/v2 v2.140.0 // indirect
	k8s.io/utils v0.0.0-20260210185600-b8788abfbbc2 // indirect
)

replace pullrun/protoapi => ../../proto-go

exclude google.golang.org/genproto v0.0.0-20220502173005-c8bf987b8c21
