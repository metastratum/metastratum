package provider

import (
	"context"
	"fmt"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

// ------------------------------------------------------------------ image

type imageDataSource struct{ dsBase }

func newImageDataSource() datasource.DataSource { return &imageDataSource{} }

type imageModel struct {
	Name     types.String `tfsdk:"name"`
	TenantID types.String `tfsdk:"tenant_id"`
	ID       types.String `tfsdk:"id"`
}

func (d *imageDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_image"
}

func (d *imageDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An image VMs can be created from, found by name.",
		Attributes: map[string]schema.Attribute{
			"name": schema.StringAttribute{Required: true},
			"tenant_id": schema.StringAttribute{
				Description: "Whose images to look among (their own and those shared with them); the provider's `tenant_id`, or the token's tenant, by default.",
				Optional:    true,
			},
			"id": schema.StringAttribute{Computed: true},
		},
	}
}

func (d *imageDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m imageModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	params := &client.ListImagesParams{}
	tenant := d.data.tenant
	if !m.TenantID.IsNull() {
		tenant = m.TenantID.ValueString()
	}
	if tenant != "" {
		id := parseID(path.Root("tenant_id"), types.StringValue(tenant), &resp.Diagnostics)
		params.TenantId = &id
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := d.data.api.ListImagesWithResponse(ctx, params)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "list images", err)
		return
	}
	for _, i := range *res.JSON200 {
		if i.Name == m.Name.ValueString() {
			m.ID = idValue(i.Id)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.Diagnostics.AddError("No such image", fmt.Sprintf("there is no image named %q", m.Name.ValueString()))
}

// ------------------------------------------------------- external network

type externalNetworkDataSource struct{ dsBase }

func newExternalNetworkDataSource() datasource.DataSource { return &externalNetworkDataSource{} }

type externalNetworkModel struct {
	Name     types.String `tfsdk:"name"`
	TenantID types.String `tfsdk:"tenant_id"`
	ID       types.String `tfsdk:"id"`
	Cidr     types.String `tfsdk:"cidr"`
}

func (d *externalNetworkDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_external_network"
}

func (d *externalNetworkDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An external network a tenant may use (for VPCs, floating IPs and load balancers), found by name.",
		Attributes: map[string]schema.Attribute{
			"name": schema.StringAttribute{Required: true},
			"tenant_id": schema.StringAttribute{
				Description: "The tenant that uses it; the provider's `tenant_id`, or the token's tenant, by default.",
				Optional:    true,
			},
			"id":   schema.StringAttribute{Computed: true},
			"cidr": schema.StringAttribute{Computed: true},
		},
	}
}

func (d *externalNetworkDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m externalNetworkModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	tenant := d.data.tenant
	if !m.TenantID.IsNull() {
		tenant = m.TenantID.ValueString()
	}
	if tenant == "" {
		me, err := d.data.api.MeWithResponse(ctx)
		if err == nil {
			err = client.Check(me.StatusCode(), me.Body, http.StatusOK)
		}
		if err != nil {
			apiError(&resp.Diagnostics, "find the token's tenant", err)
			return
		}
		tenant = me.JSON200.TenantId.String()
	}
	id := parseID(path.Root("tenant_id"), types.StringValue(tenant), &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := d.data.api.UsableExternalNetworksWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "list external networks", err)
		return
	}
	for _, e := range *res.JSON200 {
		if e.Name == m.Name.ValueString() {
			m.ID = idValue(e.Id)
			m.Cidr = types.StringValue(e.Cidr)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.Diagnostics.AddError("No such external network",
		fmt.Sprintf("no external network named %q is available to the tenant", m.Name.ValueString()))
}
