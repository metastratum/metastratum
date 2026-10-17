package client

// The controller writes its OpenAPI document; the client is generated from it.
//go:generate sh -c "cargo run -q -p mst-controller -- openapi > openapi.json"
//go:generate go tool oapi-codegen -config oapi-codegen.yaml openapi.json
