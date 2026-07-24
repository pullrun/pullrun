module pullrun/controlplane

go 1.25.0

require (
	google.golang.org/grpc v1.82.1
	pullrun/protoapi v0.0.0-00010101000000-000000000000
)

require (
	golang.org/x/net v0.56.0 // indirect
	golang.org/x/sys v0.46.0 // indirect
	golang.org/x/text v0.38.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260414002931-afd174a4e478 // indirect
	google.golang.org/protobuf v1.36.11 // indirect
)

replace pullrun/protoapi => ../proto-go
