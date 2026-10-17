package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type floatingIPResource struct{ base }

func newFloatingIPResource() resource.Resource { return &floatingIPResource{} }

type floatingIPModel struct {
	ID                types.String `tfsdk:"id"`
	TenantID          types.String `tfsdk:"tenant_id"`
	ExternalNetworkID types.String `tfsdk:"external_network_id"`
	NicID             types.String `tfsdk:"nic_id"`
	IP                types.String `tfsdk:"ip"`
}

func (r *floatingIPResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_floating_ip"
}

func (r *floatingIPResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An address on an external network that forwards to a VM's NIC. The NIC's VPC must route to that external network.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"external_network_id": schema.StringAttribute{
				Required:      true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"nic_id": schema.StringAttribute{
				Description: "The NIC it forwards to (a VM's `nic_id`); none leaves it unattached.",
				Optional:    true,
			},
			"ip": schema.StringAttribute{
				Computed:      true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
		},
	}
}

func (m *floatingIPModel) set(f *client.FloatingIpView) {
	m.ID = idValue(f.Id)
	m.TenantID = idValue(f.TenantId)
	m.ExternalNetworkID = idValue(f.ExternalNetworkId)
	m.NicID = optIDValue(f.NicId)
	m.IP = types.StringValue(f.Ip)
}

func (r *floatingIPResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan floatingIPModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	body := client.AllocateFloatingIp{
		ExternalNetworkId: parseID(path.Root("external_network_id"), plan.ExternalNetworkID, &resp.Diagnostics),
		NicId:             optID(path.Root("nic_id"), plan.NicID, &resp.Diagnostics),
		TenantId:          r.tenant(plan.TenantID, &resp.Diagnostics),
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.AllocateFloatingIpWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "allocate the floating IP", err)
		return
	}
	plan.set(res.JSON201)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *floatingIPResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state floatingIPModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.GetFloatingIpWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the floating IP", err)
		return
	}
	state.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *floatingIPResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state floatingIPModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	// By hand: a null NIC detaches it, which the generated body cannot say.
	var nic any
	if !plan.NicID.IsNull() {
		nic = plan.NicID.ValueString()
	}
	buf, _ := json.Marshal(map[string]any{"nic_id": nic})
	res, err := r.data.api.UpdateFloatingIpWithBodyWithResponse(ctx, id, "application/json", bytes.NewReader(buf))
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "update the floating IP", err)
		return
	}
	plan.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *floatingIPResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state floatingIPModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.ReleaseFloatingIpWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "release the floating IP", err)
	}
}

func (r *floatingIPResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}
