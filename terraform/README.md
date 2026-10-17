# Terraform provider for metastratum

Manages VMs, VPCs, networks, security groups, floating IPs and load balancers
on a metastratum installation through its API.

| Resource | |
|---|---|
| `metastratum_vm` | A VM from an image, or a clone of a stopped VM or template (`clone_of`). CPUs, memory, live migration and the template setting change in place: the VM is shut down for it (powered off if it has not shut down after two minutes) and started again. The disk grows in place; a smaller one replaces the VM. Security groups change at once. |
| `metastratum_vpc` | A VPC, optionally connected to an external network. |
| `metastratum_network` | A network in a VPC. |
| `metastratum_security_group`, `metastratum_security_group_rule` | A security group and its rules (a NIC in a group passes what its rules allow). |
| `metastratum_floating_ip` | An external address forwarding to a VM's `nic_id`. |
| `metastratum_load_balancer` | An external or internal address whose `listener`s spread connections over `member_vm_ids`. |

Data sources: `metastratum_image` and `metastratum_external_network`, by
name. Everything can be imported by ID (security group rules as
`<group ID>/<rule ID>`). See [examples/web](examples/web/main.tf).

## Configuration

```hcl
provider "metastratum" {
  endpoint  = "https://mst.example.com" # or MST_ENDPOINT
  token     = var.mst_token             # or MST_TOKEN: an API token (Account → API tokens)
  tenant_id = "..."                     # optional, or MST_TENANT_ID: where resources go by default
}
```

Resources take a `tenant_id` too; by default they go in the provider's, or
else the token's tenant. A token scoped to a tenant (and its roles) limits
what Terraform may do.

## Building

It is not in the Terraform Registry yet: build it, and point Terraform at
the build.

```sh
go build -o terraform-provider-metastratum .
```

In `~/.terraformrc`:

```hcl
provider_installation {
  dev_overrides {
    "metastratum/metastratum" = "/path/to/metastratum/terraform"
  }
  direct {}
}
```

## Development

The API client in `internal/client` is generated from the controller's
OpenAPI document: after changing the API, run `go generate ./...` here (it
builds the controller to export the document; CI checks the client is
current).

`go test ./...` checks the provider's schemas. The acceptance test builds a
small web setup on a running installation and changes it, as the lab has it
after its e2e tests (an image `e2e-debian-13-shared`, an external network
`lab-external`):

```sh
TF_ACC=1 MST_ENDPOINT=http://127.0.0.1:18080 MST_TOKEN=... go test ./internal/provider -run TestAccLab
```

It needs `terraform` on `PATH` (or `TF_ACC_TERRAFORM_PATH`); without it,
the test framework downloads it from HashiCorp.
