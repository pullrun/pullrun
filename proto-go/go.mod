// Module: nimbus/protoapi
// Contains Go bindings for all .proto files in /Users/YACINE/nimbus/proto/nimbus/.
// This is a shared library imported by the three Go binaries (nimbusctl,
// controlplane, nimbus-cri) via `replace` directives in their go.mod.
module nimbus/protoapi

go 1.22

require (
	google.golang.org/grpc v1.64.0
	google.golang.org/protobuf v1.34.1
)

require (
	golang.org/x/net v0.22.0 // indirect
	golang.org/x/sys v0.18.0 // indirect
	golang.org/x/text v0.14.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20240318140521-94a12d6c2237 // indirect
)
