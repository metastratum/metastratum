package provider

import (
	"context"
	"fmt"
	"net/http"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int32planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type securityGroupResource struct{ base }

func newSecurityGroupResource() resource.Resource { return &securityGroupResource{} }

type securityGroupModel struct {
	ID          types.String `tfsdk:"id"`
	TenantID    types.String `tfsdk:"tenant_id"`
	Name        types.String `tfsdk:"name"`
	Description types.String `tfsdk:"description"`
}

func (r *securityGroupResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_security_group"
}

func (r *securityGroupResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A security group: NICs in it pass the traffic its rules allow (and the replies); " +
			"NICs in no group pass none. Rules are `metastratum_security_group_rule` resources.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"name":      schema.StringAttribute{Required: true},
			"description": schema.StringAttribute{
				Optional: true,
				Computed: true,
				Default:  stringDefault(""),
			},
		},
	}
}

func (m *securityGroupModel) set(g *client.SecurityGroupView) {
	m.ID = idValue(g.Id)
	m.TenantID = idValue(g.TenantId)
	m.Name = types.StringValue(g.Name)
	m.Description = types.StringValue(g.Description)
}

func (r *securityGroupResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan securityGroupModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	desc := plan.Description.ValueString()
	body := client.CreateGroup{
		Name:        plan.Name.ValueString(),
		Description: &desc,
		TenantId:    r.tenant(plan.TenantID, &resp.Diagnostics),
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.CreateGroupWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "create the security group", err)
		return
	}
	plan.set(res.JSON201)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *securityGroupResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state securityGroupModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.GetGroupWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the security group", err)
		return
	}
	state.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *securityGroupResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state securityGroupModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	name, desc := plan.Name.ValueString(), plan.Description.ValueString()
	res, err := r.data.api.UpdateGroupWithResponse(ctx, id, client.UpdateGroup{Name: &name, Description: &desc})
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "update the security group", err)
		return
	}
	plan.set(res.JSON200)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *securityGroupResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state securityGroupModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteGroupWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the security group", err)
	}
}

func (r *securityGroupResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}

// ------------------------------------------------------------------ rules

type securityGroupRuleResource struct{ base }

func newSecurityGroupRuleResource() resource.Resource { return &securityGroupRuleResource{} }

type ruleModel struct {
	ID              types.String `tfsdk:"id"`
	SecurityGroupID types.String `tfsdk:"security_group_id"`
	Direction       types.String `tfsdk:"direction"`
	Protocol        types.String `tfsdk:"protocol"`
	PortMin         types.Int32  `tfsdk:"port_min"`
	PortMax         types.Int32  `tfsdk:"port_max"`
	RemoteCidr      types.String `tfsdk:"remote_cidr"`
	RemoteGroupID   types.String `tfsdk:"remote_group_id"`
	Description     types.String `tfsdk:"description"`
}

func (r *securityGroupRuleResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_security_group_rule"
}

func (r *securityGroupRuleResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	replaceInt := []planmodifier.Int32{int32planmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A rule of a security group: traffic it allows (stateful: replies pass too). Changing any attribute replaces it.",
		Attributes: map[string]schema.Attribute{
			"id":                idAttribute(),
			"security_group_id": schema.StringAttribute{Required: true, PlanModifiers: replace},
			"direction": schema.StringAttribute{
				Description:   "`ingress` (to the VMs) or `egress` (from them).",
				Required:      true,
				PlanModifiers: replace,
			},
			"protocol": schema.StringAttribute{
				Description:   "`any`, `tcp`, `udp` or `icmp`.",
				Required:      true,
				PlanModifiers: replace,
			},
			"port_min": schema.Int32Attribute{
				Description:   "First port (tcp and udp only).",
				Optional:      true,
				PlanModifiers: replaceInt,
			},
			"port_max": schema.Int32Attribute{
				Description: "Last port; `port_min` by default.",
				Optional:    true,
				Computed:    true,
				PlanModifiers: []planmodifier.Int32{
					int32planmodifier.RequiresReplace(),
					int32planmodifier.UseStateForUnknown(),
				},
			},
			"remote_cidr": schema.StringAttribute{
				Description:   "The other end, e.g. `0.0.0.0/0`. Give this or `remote_group_id`.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"remote_group_id": schema.StringAttribute{
				Description:   "The members of this security group as the other end.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"description": schema.StringAttribute{
				Optional:      true,
				Computed:      true,
				Default:       stringDefault(""),
				PlanModifiers: replace,
			},
		},
	}
}

func optInt32(v types.Int32) *int32 {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	i := v.ValueInt32()
	return &i
}

func int32Value(v *int32) types.Int32 {
	if v == nil {
		return types.Int32Null()
	}
	return types.Int32Value(*v)
}

func (m *ruleModel) set(group string, rv *client.RuleView) {
	m.ID = idValue(rv.Id)
	m.SecurityGroupID = types.StringValue(group)
	m.Direction = types.StringValue(rv.Direction)
	m.Protocol = types.StringValue(rv.Protocol)
	m.PortMin = int32Value(rv.PortMin)
	m.PortMax = int32Value(rv.PortMax)
	m.RemoteCidr = optString(rv.RemoteCidr)
	m.RemoteGroupID = optIDValue(rv.RemoteGroupId)
	m.Description = types.StringValue(rv.Description)
}

// rule finds a rule in its group; nil when either is gone.
func (r *securityGroupRuleResource) rule(ctx context.Context, group, rule string) (*client.RuleView, error) {
	gid, err := parseUUID(group)
	if err != nil {
		return nil, err
	}
	res, err := r.data.api.GetGroupWithResponse(ctx, gid)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if client.IsNotFound(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	for i := range res.JSON200.Rules {
		if res.JSON200.Rules[i].Id.String() == rule {
			return &res.JSON200.Rules[i], nil
		}
	}
	return nil, nil
}

func (r *securityGroupRuleResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan ruleModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	desc := plan.Description.ValueString()
	body := client.RuleSpec{
		Direction:     plan.Direction.ValueString(),
		Protocol:      plan.Protocol.ValueString(),
		PortMin:       optInt32(plan.PortMin),
		PortMax:       optInt32(plan.PortMax),
		RemoteCidr:    plan.RemoteCidr.ValueStringPointer(),
		RemoteGroupId: optID(path.Root("remote_group_id"), plan.RemoteGroupID, &resp.Diagnostics),
		Description:   &desc,
	}
	gid := parseID(path.Root("security_group_id"), plan.SecurityGroupID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	// The answer is the whole group: the new rule is one that was not
	// there before and says what this one says (rules of one group may be
	// added at the same time).
	before, err := r.data.api.GetGroupWithResponse(ctx, gid)
	if err == nil {
		err = client.Check(before.StatusCode(), before.Body, http.StatusOK)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the security group", err)
		return
	}
	known := map[string]bool{}
	for _, rv := range before.JSON200.Rules {
		known[rv.Id.String()] = true
	}
	res, err := r.data.api.AddRuleWithResponse(ctx, gid, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "add the rule", err)
		return
	}
	for i := range res.JSON201.Rules {
		rv := &res.JSON201.Rules[i]
		if !known[rv.Id.String()] && sameRule(rv, &body) {
			plan.set(plan.SecurityGroupID.ValueString(), rv)
			resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
			return
		}
	}
	resp.Diagnostics.AddError("Rule not found", "The rule was added, but is not among the group's rules.")
}

func sameRule(rv *client.RuleView, spec *client.RuleSpec) bool {
	eqInt := func(a, b *int32) bool { return (a == nil) == (b == nil) && (a == nil || *a == *b) }
	eqStr := func(a, b *string) bool { return (a == nil) == (b == nil) && (a == nil || *a == *b) }
	portMax := spec.PortMax
	if portMax == nil {
		portMax = spec.PortMin
	}
	sameGroup := (rv.RemoteGroupId == nil) == (spec.RemoteGroupId == nil) &&
		(rv.RemoteGroupId == nil || *rv.RemoteGroupId == *spec.RemoteGroupId)
	return rv.Direction == spec.Direction && rv.Protocol == spec.Protocol &&
		eqInt(rv.PortMin, spec.PortMin) && eqInt(rv.PortMax, portMax) &&
		(spec.RemoteCidr == nil || eqStr(rv.RemoteCidr, spec.RemoteCidr)) && sameGroup
}

func (r *securityGroupRuleResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state ruleModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	rv, err := r.rule(ctx, state.SecurityGroupID.ValueString(), state.ID.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "read the rule", err)
		return
	}
	if rv == nil {
		resp.State.RemoveResource(ctx)
		return
	}
	state.set(state.SecurityGroupID.ValueString(), rv)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

// Update is never called: every attribute replaces the rule.
func (r *securityGroupRuleResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.Append(resp.State.Set(ctx, req.Plan.Raw)...)
}

func (r *securityGroupRuleResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state ruleModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteRuleWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the rule", err)
	}
}

// ImportState takes `<security group ID>/<rule ID>`.
func (r *securityGroupRuleResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	group, rule, ok := strings.Cut(req.ID, "/")
	if !ok {
		resp.Diagnostics.AddError("Bad import ID",
			fmt.Sprintf("expected <security group ID>/<rule ID>, got %q", req.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("security_group_id"), group)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), rule)...)
}
