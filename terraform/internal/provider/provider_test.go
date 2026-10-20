package provider

import (
	"context"
	"fmt"
	"github.com/hashicorp/terraform-plugin-framework/types"
	"os"
	"testing"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/statecheck"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"
)

var factories = map[string]func() (tfprotov6.ProviderServer, error){
	"metastratum": providerserver.NewProtocol6WithError(New("test")()),
}

// Every schema is valid: the framework checks them when they are served.
func TestSchemas(t *testing.T) {
	server, err := factories["metastratum"]()
	if err != nil {
		t.Fatal(err)
	}
	resp, err := server.GetProviderSchema(context.Background(), &tfprotov6.GetProviderSchemaRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, d := range resp.Diagnostics {
		t.Errorf("%s: %s", d.Summary, d.Detail)
	}
	for _, name := range []string{
		"metastratum_vpc", "metastratum_network", "metastratum_security_group",
		"metastratum_security_group_rule", "metastratum_floating_ip", "metastratum_vm",
		"metastratum_load_balancer", "metastratum_disk", "metastratum_disk_attachment",
	} {
		if resp.ResourceSchemas[name] == nil {
			t.Errorf("no resource %s", name)
		}
	}
	for _, name := range []string{"metastratum_image", "metastratum_external_network"} {
		if resp.DataSourceSchemas[name] == nil {
			t.Errorf("no data source %s", name)
		}
	}
}

// TestAccLab builds a small web setup on a running installation and changes
// it: TF_ACC=1 with MST_ENDPOINT and MST_TOKEN set, an image named
// MST_TF_IMAGE (default e2e-debian-13-shared) and an external network named
// MST_TF_EXTERNAL (default lab-external), as the lab has after its e2e tests.
func TestAccLab(t *testing.T) {
	if os.Getenv("TF_ACC") == "" {
		t.Skip("TF_ACC not set")
	}
	run := fmt.Sprintf("tf-%x", time.Now().Unix())
	env := func(k, def string) string {
		if v := os.Getenv(k); v != "" {
			return v
		}
		return def
	}
	image, external := env("MST_TF_IMAGE", "e2e-debian-13-shared"), env("MST_TF_EXTERNAL", "lab-external")
	config := func(disk, memory, port int, health, attached bool) string {
		nic := "null"
		if attached {
			nic = "metastratum_vm.web.nic_id"
		}
		return fmt.Sprintf(`
data "metastratum_image" "debian" {
  name = %[2]q
}

data "metastratum_external_network" "lab" {
  name = %[3]q
}

resource "metastratum_vpc" "web" {
  name                = %[1]q
  external_network_id = data.metastratum_external_network.lab.id
}

resource "metastratum_network" "web" {
  vpc_id = metastratum_vpc.web.id
  name   = %[1]q
  cidr   = "10.61.0.0/24"
}

resource "metastratum_security_group" "web" {
  name = "%[1]s-web"
}

resource "metastratum_security_group_rule" "http" {
  security_group_id = metastratum_security_group.web.id
  direction         = "ingress"
  protocol          = "tcp"
  port_min          = %[6]d
  remote_cidr       = "0.0.0.0/0"
}

resource "metastratum_vm" "web" {
  name               = "%[1]s-web"
  image_id           = data.metastratum_image.debian.id
  vcpus              = 1
  max_vcpus          = 2
  memory_mib         = %[5]d
  max_memory_mib     = 1024
  disk_gib           = %[4]d
  network_id         = metastratum_network.web.id
  security_group_ids = [metastratum_security_group.web.id]
  password           = "tf-password"
}

resource "metastratum_floating_ip" "web" {
  external_network_id = data.metastratum_external_network.lab.id
  nic_id              = %[8]s
}

resource "metastratum_disk" "data" {
  name     = "%[1]s-data"
  size_gib = %[9]d
  host_id  = metastratum_vm.web.host_id
}

resource "metastratum_disk_attachment" "data" {
  vm_id   = metastratum_vm.web.id
  disk_id = metastratum_disk.data.id
}

resource "metastratum_load_balancer" "web" {
  name          = %[1]q
  vpc_id        = metastratum_vpc.web.id
  health_check  = %[7]t
  member_vm_ids = [metastratum_vm.web.id]

  listener {
    protocol    = "tcp"
    port        = 80
    target_port = %[6]d
  }
}
`, run, image, external, disk, memory, port, health, nic, disk-1)
	}
	resource.Test(t, resource.TestCase{
		ProtoV6ProviderFactories: factories,
		Steps: []resource.TestStep{
			{
				Config: config(3, 512, 8080, true, true),
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("running"), knownvalue.Bool(true)),
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("disk_gib"), knownvalue.Int64Exact(3)),
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("max_memory_mib"), knownvalue.Int64Exact(1024)),
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("max_vcpus"), knownvalue.Int32Exact(2)),
					statecheck.ExpectKnownValue("metastratum_load_balancer.web", tfjsonpath.New("listener").AtSliceIndex(0).AtMapKey("target_port"), knownvalue.Int32Exact(8080)),
					statecheck.ExpectKnownValue("metastratum_disk_attachment.data", tfjsonpath.New("guest_path"), knownvalue.NotNull()),
				},
			},
			{
				// In place: the disk grows, memory changes within its
				// maximum (while it runs, once the guest's balloon reports;
				// stopped for it before), the load balancer and floating IP
				// change; the rule is replaced.
				Config: config(4, 768, 8081, false, false),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("metastratum_vm.web", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("metastratum_load_balancer.web", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("metastratum_floating_ip.web", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("metastratum_security_group_rule.http", plancheck.ResourceActionReplace),
						plancheck.ExpectResourceAction("metastratum_disk.data", plancheck.ResourceActionUpdate),
						plancheck.ExpectResourceAction("metastratum_disk_attachment.data", plancheck.ResourceActionNoop),
					},
				},
				ConfigStateChecks: []statecheck.StateCheck{
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("disk_gib"), knownvalue.Int64Exact(4)),
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("memory_mib"), knownvalue.Int64Exact(768)),
					statecheck.ExpectKnownValue("metastratum_vm.web", tfjsonpath.New("running"), knownvalue.Bool(true)),
					statecheck.ExpectKnownValue("metastratum_floating_ip.web", tfjsonpath.New("nic_id"), knownvalue.Null()),
					statecheck.ExpectKnownValue("metastratum_disk.data", tfjsonpath.New("size_gib"), knownvalue.Int64Exact(3)),
				},
			},
			{
				ResourceName:      "metastratum_disk.data",
				ImportState:       true,
				ImportStateVerify: true,
			},
			{
				ResourceName:      "metastratum_disk_attachment.data",
				ImportState:       true,
				ImportStateVerify: true,
			},
			{
				ResourceName:            "metastratum_vm.web",
				ImportState:             true,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"password"},
			},
			{
				ResourceName:      "metastratum_load_balancer.web",
				ImportState:       true,
				ImportStateVerify: true,
			},
			{
				ResourceName:      "metastratum_vpc.web",
				ImportState:       true,
				ImportStateVerify: true,
			},
		},
	})
}

func TestUnsetBoolsAreLeftToTheServer(t *testing.T) {
	if optBool(types.BoolNull()) != nil || optBool(types.BoolUnknown()) != nil {
		t.Fatal("an unset or computed bool must be sent as none")
	}
	if v := optBool(types.BoolValue(false)); v == nil || *v {
		t.Fatal("false must be sent as false")
	}
}
