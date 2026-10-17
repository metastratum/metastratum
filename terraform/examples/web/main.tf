# A web server behind a load balancer on the external network.
#
#   export MST_ENDPOINT=https://mst.example.com MST_TOKEN=...
#   terraform init && terraform apply

terraform {
  required_providers {
    metastratum = {
      source = "metastratum/metastratum"
    }
  }
}

provider "metastratum" {}

data "metastratum_image" "debian" {
  name = "debian-13"
}

data "metastratum_external_network" "public" {
  name = "public"
}

resource "metastratum_vpc" "web" {
  name                = "web"
  external_network_id = data.metastratum_external_network.public.id
}

resource "metastratum_network" "web" {
  vpc_id = metastratum_vpc.web.id
  name   = "web"
  cidr   = "10.10.0.0/24"
}

resource "metastratum_security_group" "web" {
  name = "web"
}

resource "metastratum_security_group_rule" "http" {
  security_group_id = metastratum_security_group.web.id
  direction         = "ingress"
  protocol          = "tcp"
  port_min          = 80
  remote_cidr       = "0.0.0.0/0"
}

resource "metastratum_security_group_rule" "out" {
  security_group_id = metastratum_security_group.web.id
  direction         = "egress"
  protocol          = "any"
  remote_cidr       = "0.0.0.0/0"
}

resource "metastratum_vm" "web" {
  count               = 2
  name                = "web-${count.index + 1}"
  image_id            = data.metastratum_image.debian.id
  vcpus               = 2
  memory_mib          = 2048
  disk_gib            = 20
  network_id          = metastratum_network.web.id
  security_group_ids  = [metastratum_security_group.web.id]
  ssh_authorized_keys = [file("~/.ssh/id_ed25519.pub")]
  user_data           = <<-YAML
    #cloud-config
    packages: [nginx]
  YAML
}

resource "metastratum_load_balancer" "web" {
  name          = "web"
  vpc_id        = metastratum_vpc.web.id
  member_vm_ids = metastratum_vm.web[*].id

  listener {
    protocol = "tcp"
    port     = 80
  }
}

output "address" {
  value = metastratum_load_balancer.web.ip
}
