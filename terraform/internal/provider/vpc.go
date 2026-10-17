package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type vpcResource struct{ base }

func newVpcResource() resource.Resource { return &vpcResource{} }

type vpcModel struct {
	ID                types.String `tfsdk:"id"`
	TenantID          types.String `tfsdk:"tenant_id"`
	Name              types.String `tfsdk:"name"`
	ExternalNetworkID types.String `tfsdk:"external_network_id"`
	GatewayIP         types.String `tfsdk:"gateway_ip"`
}

func (r *vpcResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_vpc"
}

func (r *vpcResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A VPC: a routing domain of networks, optionally connected to an external network (with source NAT).",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"name": schema.StringAttribute{
				Description: "Unique within the tenant.",
				Required:    true,
			},
			"external_network_id": schema.StringAttribute{
				Description: "The external network its traffic leaves through; none keeps it internal.",
				Optional:    true,
			},
			"gateway_ip": schema.StringAttribute{
				Description: "Its address on the external network.",
				Computed:    true,
			},
		},
	}
}

// idAttribute is every resource's computed ID.
func idAttribute() schema.StringAttribute {
	return schema.StringAttribute{
		Computed:      true,
		PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
	}
}

// tenantAttribute is the tenant a resource belongs to: given, the
// provider's, or the token's.
func tenantAttribute() schema.StringAttribute {
	return schema.StringAttribute{
		Description: "The tenant it belongs to; the provider's `tenant_id`, or the token's tenant, by default.",
		Optional:    true,
		Computed:    true,
		PlanModifiers: []planmodifier.String{
			stringplanmodifier.RequiresReplace(),
			stringplanmodifier.UseStateForUnknown(),
		},
	}
}

func (m *vpcModel) set(v *client.VpcView) {
	m.ID = idValue(v.Id)
	m.TenantID = idValue(v.TenantId)
	m.Name = types.StringValue(v.Name)
	m.ExternalNetworkID = optIDValue(v.ExternalNetworkId)
	m.GatewayIP = optString(v.GatewayIp)
}

func (r *vpcResource) get(ctx context.Context, id string, diags *diag.Diagnostics) (*client.VpcView, error) {
	vid := parseID(path.Root("id"), types.StringValue(id), diags)
	if diags.HasError() {
		return nil, nil
	}
	res, err := r.data.api.GetVpcWithResponse(ctx, vid)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		return nil, err
	}
	return res.JSON200, nil
}

func (r *vpcResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan vpcModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	body := client.CreateVpc{
		Name:              plan.Name.ValueString(),
		TenantId:          r.tenant(plan.TenantID, &resp.Diagnostics),
		ExternalNetworkId: optID(path.Root("external_network_id"), plan.ExternalNetworkID, &resp.Diagnostics),
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.CreateVpcWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "create the VPC", err)
		return
	}
	plan.set(res.JSON201)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *vpcResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state vpcModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	v, err := r.get(ctx, state.ID.ValueString(), &resp.Diagnostics)
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the VPC", err)
		return
	}
	if v == nil {
		return
	}
	state.set(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *vpcResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state vpcModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	// Written by hand: a null external network disconnects the VPC, which
	// the generated body cannot say.
	patch := map[string]any{"name": plan.Name.ValueString()}
	if !plan.ExternalNetworkID.Equal(state.ExternalNetworkID) {
		if plan.ExternalNetworkID.IsNull() {
			patch["external_network_id"] = nil
		} else {
			patch["external_network_id"] = plan.ExternalNetworkID.ValueString()
		}
	}
	buf, _ := json.Marshal(patch)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	res, err := r.data.api.UpdateVpcWithBodyWithResponse(ctx, id, "application/json", bytes.NewReader(buf))
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "update the VPC", err)
		return
	}
	plan.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *vpcResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state vpcModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteVpcWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the VPC", err)
	}
}

func (r *vpcResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}

// ---------------------------------------------------------------- networks

type networkResource struct{ base }

func newNetworkResource() resource.Resource { return &networkResource{} }

type networkModel struct {
	ID       types.String `tfsdk:"id"`
	TenantID types.String `tfsdk:"tenant_id"`
	VpcID    types.String `tfsdk:"vpc_id"`
	Name     types.String `tfsdk:"name"`
	Cidr     types.String `tfsdk:"cidr"`
	Gateway  types.String `tfsdk:"gateway"`
}

func (r *networkResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_network"
}

func (r *networkResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A network (subnet) in a VPC, with DHCP and the VPC's router as gateway.",
		Attributes: map[string]schema.Attribute{
			"id": idAttribute(),
			"tenant_id": schema.StringAttribute{
				Description: "The VPC's tenant.",
				Computed:    true,
			},
			"vpc_id": schema.StringAttribute{Required: true, PlanModifiers: replace},
			"name":   schema.StringAttribute{Required: true, PlanModifiers: replace},
			"cidr": schema.StringAttribute{
				Description:   "e.g. `10.1.0.0/24`; /16 to /29, not overlapping the VPC's other networks.",
				Required:      true,
				PlanModifiers: replace,
			},
			"gateway": schema.StringAttribute{
				Description: "The router's address in the network; the first one by default.",
				Optional:    true,
				Computed:    true,
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.RequiresReplace(),
					stringplanmodifier.UseStateForUnknown(),
				},
			},
		},
	}
}

func (m *networkModel) set(n *client.NetworkView) {
	m.ID = idValue(n.Id)
	m.TenantID = idValue(n.TenantId)
	m.VpcID = idValue(n.VpcId)
	m.Name = types.StringValue(n.Name)
	m.Cidr = types.StringValue(n.Cidr)
	m.Gateway = types.StringValue(n.Gateway)
}

func (r *networkResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan networkModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	body := client.CreateNetwork{
		VpcId: parseID(path.Root("vpc_id"), plan.VpcID, &resp.Diagnostics),
		Name:  plan.Name.ValueString(),
		Cidr:  plan.Cidr.ValueString(),
	}
	if !plan.Gateway.IsNull() && !plan.Gateway.IsUnknown() {
		g := plan.Gateway.ValueString()
		body.Gateway = &g
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.CreateNetworkWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "create the network", err)
		return
	}
	plan.set(res.JSON201)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *networkResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state networkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.GetNetworkWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the network", err)
		return
	}
	state.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

// Update is never called: every attribute replaces the network.
func (r *networkResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.Append(resp.State.Set(ctx, req.Plan.Raw)...)
}

func (r *networkResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state networkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteNetworkWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the network", err)
	}
}

func (r *networkResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}
