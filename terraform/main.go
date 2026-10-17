// Command terraform-provider-metastratum is the metastratum Terraform
// provider.
package main

import (
	"context"
	"flag"
	"log"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"

	"github.com/metastratum/metastratum/terraform/internal/provider"
)

// Set at build time.
var version = "dev"

func main() {
	var debug bool
	flag.BoolVar(&debug, "debug", false, "run with support for debuggers like delve")
	flag.Parse()
	err := providerserver.Serve(context.Background(), provider.New(version), providerserver.ServeOpts{
		Address: "registry.terraform.io/metastratum/metastratum",
		Debug:   debug,
	})
	if err != nil {
		log.Fatal(err)
	}
}
