module nimbus/cri

go 1.22.0

toolchain go1.24.4

require (
	google.golang.org/grpc v1.64.0
	k8s.io/cri-api v0.30.0
	nimbus/protoapi v0.0.0
)

require (
	github.com/gogo/protobuf v1.3.2 // indirect
	golang.org/x/net v0.23.0 // indirect
	golang.org/x/sys v0.18.0 // indirect
	golang.org/x/text v0.14.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20240318140521-94a12d6c2237 // indirect
	google.golang.org/protobuf v1.34.1 // indirect
)

replace nimbus/protoapi => ../../proto-go

exclude google.golang.org/genproto v0.0.0-20220502173005-c8bf987b8c21
