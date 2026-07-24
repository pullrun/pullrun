// Module: pullrun/protoapi
// Contains Go bindings for all .proto files in proto/pullrun/.
// This is a shared library imported by the three Go binaries (pullrun,
// controlplane, pullrun-cri) via `replace` directives in their go.mod.
module pullrun/protoapi

go 1.25.0

require (
	google.golang.org/grpc v1.82.1
	google.golang.org/protobuf v1.36.11
)

require (
	golang.org/x/net v0.56.0 // indirect
	golang.org/x/sys v0.46.0 // indirect
	golang.org/x/text v0.38.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260414002931-afd174a4e478 // indirect
)
