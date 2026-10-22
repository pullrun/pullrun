module pullrun/cli

go 1.25.0

require (
	// >= v1.18.7 required for CVE-2026-63209 / GHSA-259r-337f-4rfw.
	github.com/klauspost/compress v1.18.7
	github.com/mark3labs/mcp-go v0.40.0
	github.com/spf13/cobra v1.8.0
	golang.org/x/sys v0.47.0
	golang.org/x/term v0.45.0
	google.golang.org/grpc v1.83.2
	pullrun/protoapi v0.0.0-00010101000000-000000000000
)

require (
	github.com/bahlo/generic-list-go v0.2.0 // indirect
	github.com/buger/jsonparser v1.2.0 // indirect
	github.com/google/uuid v1.6.0 // indirect
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/invopop/jsonschema v0.13.0 // indirect
	github.com/mailru/easyjson v0.7.7 // indirect
	github.com/spf13/cast v1.7.1 // indirect
	github.com/spf13/pflag v1.0.5 // indirect
	github.com/wk8/go-ordered-map/v2 v2.1.8 // indirect
	github.com/yosida95/uritemplate/v3 v3.0.2 // indirect
	golang.org/x/net v0.58.0 // indirect
	golang.org/x/text v0.41.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260526163538-3dc84a4a5aaa // indirect
	google.golang.org/protobuf v1.36.11 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)

replace pullrun/protoapi => ../../proto-go
