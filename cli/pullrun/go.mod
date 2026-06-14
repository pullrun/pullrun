module pullrun/cli

go 1.25.0

require (
	github.com/klauspost/compress v1.18.6
	github.com/mark3labs/mcp-go v0.40.0
	github.com/spf13/cobra v1.8.0
	golang.org/x/sys v0.46.0
	golang.org/x/term v0.44.0
	google.golang.org/grpc v1.81.1
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
	golang.org/x/net v0.56.0 // indirect
	golang.org/x/text v0.38.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260226221140-a57be14db171 // indirect
	google.golang.org/protobuf v1.36.11 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)

replace pullrun/protoapi => ../../proto-go
