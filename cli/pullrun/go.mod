module pullrun/cli

go 1.24

require (
	github.com/klauspost/compress v1.18.6
	github.com/spf13/cobra v1.8.0
	golang.org/x/term v0.18.0
	google.golang.org/grpc v1.64.0
	pullrun/protoapi v0.0.0-00010101000000-000000000000
)

require (
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/spf13/pflag v1.0.5 // indirect
	golang.org/x/net v0.22.0 // indirect
	golang.org/x/sys v0.29.0 // indirect
	golang.org/x/text v0.14.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20240318140521-94a12d6c2237 // indirect
	google.golang.org/protobuf v1.34.1 // indirect
)

replace pullrun/protoapi => ../../proto-go
