package provider

import (
	"context"
	"net/http"

	"github.com/google/uuid"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/setdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type loadBalancerResource struct{ base }

func newLoadBalancerResource() resource.Resource { return &loadBalancerResource{} }

type loadBalancerModel struct {
	ID          types.String    `tfsdk:"id"`
	TenantID    types.String    `tfsdk:"tenant_id"`
	Name        types.String    `tfsdk:"name"`
	VpcID       types.String    `tfsdk:"vpc_id"`
	NetworkID   types.String    `tfsdk:"network_id"`
	HealthCheck types.Bool      `tfsdk:"health_check"`
	Listeners   []listenerModel `tfsdk:"listener"`
	MemberVMIDs types.Set       `tfsdk:"member_vm_ids"`
	IP          types.String    `tfsdk:"ip"`
}

type listenerModel struct {
	Protocol   types.String `tfsdk:"protocol"`
	Port       types.Int32  `tfsdk:"port"`
	TargetPort types.Int32  `tfsdk:"target_port"`
}

func (r *loadBalancerResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_load_balancer"
}

func (r *loadBalancerResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A load balancer: an address in a VPC whose listeners spread TCP and UDP connections over member VMs. " +
			"Members' security groups must let the clients in; connections keep the client's address.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"name":      schema.StringAttribute{Required: true},
			"vpc_id":    schema.StringAttribute{Required: true, PlanModifiers: replace},
			"network_id": schema.StringAttribute{
				Description:   "Put the address in this network of the VPC, for its VMs only; by default it is on the VPC's external network.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"health_check": schema.BoolAttribute{
				Description: "Check members and leave out those that do not answer (default true).",
				Optional:    true,
				Computed:    true,
				Default:     booldefault.StaticBool(true),
			},
			"member_vm_ids": schema.SetAttribute{
				Description: "VMs to spread connections over, each with a NIC in the VPC.",
				ElementType: types.StringType,
				Optional:    true,
				Computed:    true,
				Default:     setdefault.StaticValue(types.SetValueMust(types.StringType, nil)),
			},
			"ip": schema.StringAttribute{
				Description:   "The address clients connect to.",
				Computed:      true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
		},
		Blocks: map[string]schema.Block{
			"listener": schema.ListNestedBlock{
				Description: "A port on the address, forwarded to a port on the members.",
				NestedObject: schema.NestedBlockObject{
					Attributes: map[string]schema.Attribute{
						"protocol": schema.StringAttribute{Description: "`tcp` or `udp`.", Required: true},
						"port":     schema.Int32Attribute{Required: true},
						"target_port": schema.Int32Attribute{
							Description: "The members' port; `port` by default.",
							Optional:    true,
							Computed:    true,
						},
					},
				},
			},
		},
	}
}

func (m *loadBalancerModel) set(lb *client.LoadBalancerView, diags *diag.Diagnostics) {
	m.ID = idValue(lb.Id)
	m.TenantID = idValue(lb.TenantId)
	m.Name = types.StringValue(lb.Name)
	m.VpcID = idValue(lb.VpcId)
	m.NetworkID = optIDValue(lb.NetworkId)
	m.HealthCheck = types.BoolValue(lb.HealthCheck)
	m.IP = types.StringValue(lb.Ip)
	m.Listeners = make([]listenerModel, 0, len(lb.Listeners))
	for _, l := range lb.Listeners {
		target := l.Port
		if l.TargetPort != nil {
			target = *l.TargetPort
		}
		m.Listeners = append(m.Listeners, listenerModel{
			Protocol:   types.StringValue(l.Protocol),
			Port:       types.Int32Value(l.Port),
			TargetPort: types.Int32Value(target),
		})
	}
	members := make([]attr.Value, 0, len(lb.Members))
	for _, mv := range lb.Members {
		members = append(members, idValue(mv.VmId))
	}
	set, d := types.SetValue(types.StringType, members)
	diags.Append(d...)
	m.MemberVMIDs = set
}

func (m *loadBalancerModel) listeners() []client.ListenerSpec {
	out := make([]client.ListenerSpec, 0, len(m.Listeners))
	for _, l := range m.Listeners {
		out = append(out, client.ListenerSpec{
			Protocol:   l.Protocol.ValueString(),
			Port:       l.Port.ValueInt32(),
			TargetPort: optInt32(l.TargetPort),
		})
	}
	return out
}

func (m *loadBalancerModel) members(ctx context.Context, diags *diag.Diagnostics) []uuid.UUID {
	var strs []string
	diags.Append(m.MemberVMIDs.ElementsAs(ctx, &strs, false)...)
	out := make([]uuid.UUID, 0, len(strs))
	for _, s := range strs {
		out = append(out, parseID(path.Root("member_vm_ids"), types.StringValue(s), diags))
	}
	return out
}

func (r *loadBalancerResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan loadBalancerModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	d := &resp.Diagnostics
	listeners := plan.listeners()
	members := plan.members(ctx, d)
	body := client.CreateLoadBalancer{
		Name:        plan.Name.ValueString(),
		TenantId:    r.tenant(plan.TenantID, d),
		VpcId:       parseID(path.Root("vpc_id"), plan.VpcID, d),
		NetworkId:   optID(path.Root("network_id"), plan.NetworkID, d),
		HealthCheck: plan.HealthCheck.ValueBoolPointer(),
		Listeners:   &listeners,
		MemberVmIds: &members,
	}
	if d.HasError() {
		return
	}
	res, err := r.data.api.CreateLoadBalancerWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(d, "create the load balancer", err)
		return
	}
	plan.set(res.JSON201, d)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *loadBalancerResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state loadBalancerModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.GetLoadBalancerWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the load balancer", err)
		return
	}
	state.set(res.JSON200, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *loadBalancerResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state loadBalancerModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	d := &resp.Diagnostics
	id := parseID(path.Root("id"), state.ID, d)
	body := client.UpdateLoadBalancer{
		Name:        plan.Name.ValueString(),
		HealthCheck: plan.HealthCheck.ValueBool(),
		Listeners:   plan.listeners(),
		MemberVmIds: plan.members(ctx, d),
	}
	if d.HasError() {
		return
	}
	res, err := r.data.api.UpdateLoadBalancerWithResponse(ctx, id, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(d, "update the load balancer", err)
		return
	}
	plan.set(res.JSON200, d)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *loadBalancerResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state loadBalancerModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteLoadBalancerWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the load balancer", err)
	}
}

func (r *loadBalancerResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}
