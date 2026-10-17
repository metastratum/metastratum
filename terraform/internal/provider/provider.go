// Package provider is the metastratum Terraform provider: VMs, networks,
// security groups, floating IPs and load balancers through a controller's
// API.
package provider

import (
	"context"
	"os"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type mstProvider struct {
	version string
}

// New returns the provider, for providerserver.Serve and tests.
func New(version string) func() provider.Provider {
	return func() provider.Provider {
		return &mstProvider{version: version}
	}
}

type providerModel struct {
	Endpoint types.String `tfsdk:"endpoint"`
	Token    types.String `tfsdk:"token"`
	TenantID types.String `tfsdk:"tenant_id"`
}

// providerData is what resources and data sources get to work with.
type providerData struct {
	api *client.API
	// The tenant resources go in unless they say otherwise; empty for the
	// token's own.
	tenant string
}

func (p *mstProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = "metastratum"
	resp.Version = p.version
}

func (p *mstProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "Manages virtual machines, networks and load balancers on a metastratum installation.",
		Attributes: map[string]schema.Attribute{
			"endpoint": schema.StringAttribute{
				Description: "The controller's address, e.g. `https://mst.example.com`. Defaults to `MST_ENDPOINT`.",
				Optional:    true,
			},
			"token": schema.StringAttribute{
				Description: "An API token (Account → API tokens). Defaults to `MST_TOKEN`.",
				Optional:    true,
				Sensitive:   true,
			},
			"tenant_id": schema.StringAttribute{
				Description: "The tenant resources are created in unless they name one; the token's own by default. Defaults to `MST_TENANT_ID`.",
				Optional:    true,
			},
		},
	}
}

func (p *mstProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var cfg providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &cfg)...)
	if resp.Diagnostics.HasError() {
		return
	}
	value := func(v types.String, env string) string {
		if !v.IsNull() && !v.IsUnknown() {
			return v.ValueString()
		}
		return os.Getenv(env)
	}
	endpoint := value(cfg.Endpoint, "MST_ENDPOINT")
	token := value(cfg.Token, "MST_TOKEN")
	if endpoint == "" {
		resp.Diagnostics.AddAttributeError(path.Root("endpoint"), "No endpoint",
			"Set the provider's endpoint, or MST_ENDPOINT, to the controller's address.")
	}
	if token == "" {
		resp.Diagnostics.AddAttributeError(path.Root("token"), "No API token",
			"Set the provider's token, or MST_TOKEN, to an API token.")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	api, err := client.New(endpoint, token, "terraform-provider-metastratum/"+p.version)
	if err != nil {
		resp.Diagnostics.AddError("Cannot use the endpoint", err.Error())
		return
	}
	data := &providerData{api: api, tenant: value(cfg.TenantID, "MST_TENANT_ID")}
	resp.ResourceData = data
	resp.DataSourceData = data
}

func (p *mstProvider) Resources(_ context.Context) []func() resource.Resource {
	return []func() resource.Resource{
		newVpcResource,
		newNetworkResource,
		newSecurityGroupResource,
		newSecurityGroupRuleResource,
		newFloatingIPResource,
		newVMResource,
		newLoadBalancerResource,
	}
}

func (p *mstProvider) DataSources(_ context.Context) []func() datasource.DataSource {
	return []func() datasource.DataSource{
		newImageDataSource,
		newExternalNetworkDataSource,
	}
}
