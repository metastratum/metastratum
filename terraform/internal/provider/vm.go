package provider

import (
	"context"
	"fmt"
	"net/http"
	"time"

	"github.com/google/uuid"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int32planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/setplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

const gib = 1 << 30

// How long VM work may take: creating one copies its disk.
const vmTimeout = 30 * time.Minute

type vmResource struct{ base }

func newVMResource() resource.Resource { return &vmResource{} }

type vmModel struct {
	ID                types.String `tfsdk:"id"`
	TenantID          types.String `tfsdk:"tenant_id"`
	Name              types.String `tfsdk:"name"`
	ImageID           types.String `tfsdk:"image_id"`
	CloneOf           types.String `tfsdk:"clone_of"`
	Vcpus             types.Int32  `tfsdk:"vcpus"`
	MemoryMib         types.Int64  `tfsdk:"memory_mib"`
	DiskGib           types.Int64  `tfsdk:"disk_gib"`
	Firmware          types.String `tfsdk:"firmware"`
	NetworkID         types.String `tfsdk:"network_id"`
	SecurityGroupIDs  types.Set    `tfsdk:"security_group_ids"`
	SSHAuthorizedKeys types.List   `tfsdk:"ssh_authorized_keys"`
	Password          types.String `tfsdk:"password"`
	UserData          types.String `tfsdk:"user_data"`
	ClusterID         types.String `tfsdk:"cluster_id"`
	StoragePoolID     types.String `tfsdk:"storage_pool_id"`
	Migratable        types.Bool   `tfsdk:"migratable"`
	Running           types.Bool   `tfsdk:"running"`
	Template          types.Bool   `tfsdk:"template"`
	IP                types.String `tfsdk:"ip"`
	NicID             types.String `tfsdk:"nic_id"`
	HostID            types.String `tfsdk:"host_id"`
}

func (r *vmResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_vm"
}

func (r *vmResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	computedReplace := []planmodifier.String{
		stringplanmodifier.RequiresReplace(),
		stringplanmodifier.UseStateForUnknown(),
	}
	computed := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		Description: "A virtual machine, from an image or as a clone of another (stopped) VM or template. " +
			"Changing its CPUs, memory, live migration or template setting shuts it down for the change (powering it off if it " +
			"has not shut down after two minutes) and starts it again; " +
			"its disk grows in place, and a smaller disk replaces it.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"name": schema.StringAttribute{
				Description:   "DNS-style label, unique within the tenant; also the guest's host name.",
				Required:      true,
				PlanModifiers: replace,
			},
			"image_id": schema.StringAttribute{
				Description:   "The image its disk is made from. Give this or `clone_of`.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"clone_of": schema.StringAttribute{
				Description:   "A stopped VM or template whose disks it gets copies of, with its CPUs, memory and storage.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"vcpus": schema.Int32Attribute{
				Description:   "Virtual CPUs; required with `image_id`.",
				Optional:      true,
				Computed:      true,
				PlanModifiers: []planmodifier.Int32{int32planmodifier.UseStateForUnknown()},
			},
			"memory_mib": schema.Int64Attribute{
				Description:   "Memory in MiB; required with `image_id`.",
				Optional:      true,
				Computed:      true,
				PlanModifiers: []planmodifier.Int64{int64planmodifier.UseStateForUnknown()},
			},
			"disk_gib": schema.Int64Attribute{
				Description: "Size of the boot disk in GiB; required with `image_id`. It grows in place; making it smaller replaces the VM.",
				Optional:    true,
				Computed:    true,
				PlanModifiers: []planmodifier.Int64{
					int64planmodifier.UseStateForUnknown(),
					int64planmodifier.RequiresReplaceIf(func(_ context.Context, req planmodifier.Int64Request, resp *int64planmodifier.RequiresReplaceIfFuncResponse) {
						resp.RequiresReplace = !req.StateValue.IsNull() && !req.PlanValue.IsUnknown() &&
							req.PlanValue.ValueInt64() < req.StateValue.ValueInt64()
					}, "Disks only grow: a smaller one replaces the VM.", "Disks only grow: a smaller one replaces the VM."),
				},
			},
			"firmware": schema.StringAttribute{
				Description:   "`uefi` (default) or `bios`.",
				Optional:      true,
				Computed:      true,
				PlanModifiers: computedReplace,
			},
			"network_id": schema.StringAttribute{
				Description:   "The network its NIC is on; the tenant's default network otherwise.",
				Optional:      true,
				Computed:      true,
				PlanModifiers: computedReplace,
			},
			"security_group_ids": schema.SetAttribute{
				Description:   "Security groups of its NIC; the tenant's `default` group otherwise. Changes apply at once.",
				ElementType:   types.StringType,
				Optional:      true,
				Computed:      true,
				PlanModifiers: []planmodifier.Set{setplanmodifier.UseStateForUnknown()},
			},
			"ssh_authorized_keys": schema.ListAttribute{
				Description:   "SSH public keys for the image's default user.",
				ElementType:   types.StringType,
				Optional:      true,
				PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()},
			},
			"password": schema.StringAttribute{
				Description:   "Console password for the image's default user.",
				Optional:      true,
				Sensitive:     true,
				PlanModifiers: replace,
			},
			"user_data": schema.StringAttribute{
				Description: "Complete cloud-init user data, instead of `ssh_authorized_keys` and `password`. " +
					"Kept out of plans and logs, as it often holds secrets.",
				Optional:      true,
				Sensitive:     true,
				PlanModifiers: replace,
			},
			"cluster_id": schema.StringAttribute{
				Description:   "The cluster to place it in; any the tenant may use otherwise.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"storage_pool_id": schema.StringAttribute{
				Description:   "The storage pool for its disk; local storage on its host otherwise.",
				Optional:      true,
				PlanModifiers: replace,
			},
			"migratable": schema.BoolAttribute{
				Description:   "Whether it may live-migrate; by default when its cluster has more than one host.",
				Optional:      true,
				Computed:      true,
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.UseStateForUnknown()},
			},
			"running": schema.BoolAttribute{
				Description: "Whether it should be running (default true).",
				Optional:    true,
				Computed:    true,
				Default:     booldefault.StaticBool(true),
			},
			"template": schema.BoolAttribute{
				Description: "Keep it as a template, to be cloned and never started (needs `running = false`).",
				Optional:    true,
				Computed:    true,
				Default:     booldefault.StaticBool(false),
			},
			"ip":      schema.StringAttribute{Description: "Its NIC's address.", Computed: true, PlanModifiers: computed},
			"nic_id":  schema.StringAttribute{Description: "Its NIC, e.g. for a floating IP.", Computed: true, PlanModifiers: computed},
			"host_id": schema.StringAttribute{Description: "The host it is on.", Computed: true},
		},
	}
}

func (r *vmResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m vmModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !m.ImageID.IsUnknown() && !m.CloneOf.IsUnknown() && m.ImageID.IsNull() == m.CloneOf.IsNull() {
		resp.Diagnostics.AddError("Image or clone", "Give either `image_id` or `clone_of`.")
	}
	if !m.ImageID.IsNull() {
		for name, null := range map[string]bool{
			"vcpus": m.Vcpus.IsNull(), "memory_mib": m.MemoryMib.IsNull(), "disk_gib": m.DiskGib.IsNull(),
		} {
			if null {
				resp.Diagnostics.AddAttributeError(path.Root(name), "Missing size", name+" is required with image_id.")
			}
		}
	}
	if m.Template.ValueBool() && m.Running.ValueBool() {
		resp.Diagnostics.AddAttributeError(path.Root("running"), "A running template",
			"A template is never started: set running = false.")
	}
}

// set copies what the API says into the model; what it does not return
// (the image, keys, password, placement) stays as planned.
func (m *vmModel) set(ctx context.Context, v *client.VmView, diags *diag.Diagnostics) {
	m.ID = idValue(v.Id)
	m.TenantID = idValue(v.TenantId)
	m.Name = types.StringValue(v.Name)
	m.Vcpus = types.Int32Value(v.Vcpus)
	m.MemoryMib = types.Int64Value(v.MemoryMib)
	if len(v.Disks) > 0 {
		m.DiskGib = types.Int64Value((v.Disks[0].SizeBytes + gib - 1) / gib)
		// Imported: what it was made from is its disk's image.
		if m.ImageID.IsNull() && m.CloneOf.IsNull() {
			m.ImageID = optIDValue(v.Disks[0].ImageId)
		}
	}
	m.Firmware = types.StringValue(string(v.Firmware))
	m.Migratable = types.BoolValue(v.Migratable)
	m.Running = types.BoolValue(v.DesiredPower == "running")
	m.Template = types.BoolValue(v.Template)
	m.HostID = optIDValue(v.HostId)
	if len(v.Nics) > 0 {
		n := v.Nics[0]
		m.NetworkID = idValue(n.NetworkId)
		m.IP = types.StringValue(n.Ip)
		m.NicID = idValue(n.Id)
		groups := make([]attr.Value, 0, len(n.SecurityGroupIds))
		for _, g := range n.SecurityGroupIds {
			groups = append(groups, idValue(g))
		}
		set, d := types.SetValue(types.StringType, groups)
		diags.Append(d...)
		m.SecurityGroupIDs = set
	}
}

func (r *vmResource) get(ctx context.Context, id uuid.UUID) (*client.VmView, error) {
	res, err := r.data.api.GetVmWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		return nil, err
	}
	return res.JSON200, nil
}

// waitPower waits until the VM's host reports it running or stopped.
func (r *vmResource) waitPower(ctx context.Context, id uuid.UUID, want string, within time.Duration) error {
	deadline := time.Now().Add(within)
	for {
		v, err := r.get(ctx, id)
		if err != nil {
			return err
		}
		if v.Status == "ready" && v.Power == want {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("the VM is %s, not %s", v.Power, want)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(2 * time.Second):
		}
	}
}

// How long a guest gets to shut down before it is powered off.
const shutdownGrace = 2 * time.Minute

// power starts or stops the VM and waits until it is. A guest that does not
// shut down when asked (one still booting may miss the request) is powered
// off after shutdownGrace.
func (r *vmResource) power(ctx context.Context, id uuid.UUID, running bool) error {
	if running {
		res, err := r.data.api.StartVmWithResponse(ctx, id)
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err == nil {
			err = r.data.api.Wait(ctx, res.JSON202.TaskId)
		}
		if err != nil {
			return err
		}
		return r.waitPower(ctx, id, "running", 10*time.Minute)
	}
	stop := func(force bool) error {
		res, err := r.data.api.StopVmWithResponse(ctx, id, &client.StopVmParams{Force: &force})
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err == nil {
			err = r.data.api.Wait(ctx, res.JSON202.TaskId)
		}
		return err
	}
	if err := stop(false); err != nil {
		return err
	}
	if r.waitPower(ctx, id, "stopped", shutdownGrace) == nil {
		return nil
	}
	if err := stop(true); err != nil {
		return err
	}
	return r.waitPower(ctx, id, "stopped", 2*time.Minute)
}

func stringList(ctx context.Context, l types.List, diags *diag.Diagnostics) *[]string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	var out []string
	diags.Append(l.ElementsAs(ctx, &out, false)...)
	return &out
}

func idSet(ctx context.Context, s types.Set, diags *diag.Diagnostics) *[]uuid.UUID {
	if s.IsNull() || s.IsUnknown() {
		return nil
	}
	var strs []string
	diags.Append(s.ElementsAs(ctx, &strs, false)...)
	out := make([]uuid.UUID, 0, len(strs))
	for _, v := range strs {
		out = append(out, parseID(path.Root("security_group_ids"), types.StringValue(v), diags))
	}
	return &out
}

func (r *vmResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	ctx, cancel := context.WithTimeout(ctx, vmTimeout)
	defer cancel()
	var plan vmModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	d := &resp.Diagnostics
	// A template is made stopped, then marked.
	start := plan.Running.ValueBool() && !plan.Template.ValueBool()
	var accepted *client.AcceptedVmView
	if !plan.CloneOf.IsNull() {
		body := client.CloneVm{
			Name:              plan.Name.ValueString(),
			Start:             &start,
			ClusterId:         optID(path.Root("cluster_id"), plan.ClusterID, d),
			NetworkId:         optID(path.Root("network_id"), plan.NetworkID, d),
			SecurityGroupIds:  idSet(ctx, plan.SecurityGroupIDs, d),
			SshAuthorizedKeys: stringList(ctx, plan.SSHAuthorizedKeys, d),
			Password:          plan.Password.ValueStringPointer(),
		}
		source := parseID(path.Root("clone_of"), plan.CloneOf, d)
		if d.HasError() {
			return
		}
		res, err := r.data.api.CloneVmWithResponse(ctx, source, body)
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err != nil {
			apiError(d, "clone the VM", err)
			return
		}
		accepted = res.JSON202
	} else {
		body := client.CreateVm{
			Name:              plan.Name.ValueString(),
			TenantId:          r.tenant(plan.TenantID, d),
			ImageId:           parseID(path.Root("image_id"), plan.ImageID, d),
			Vcpus:             plan.Vcpus.ValueInt32(),
			MemoryMib:         plan.MemoryMib.ValueInt64(),
			DiskGib:           plan.DiskGib.ValueInt64(),
			Start:             &start,
			NetworkId:         optID(path.Root("network_id"), plan.NetworkID, d),
			SecurityGroupIds:  idSet(ctx, plan.SecurityGroupIDs, d),
			SshAuthorizedKeys: stringList(ctx, plan.SSHAuthorizedKeys, d),
			Password:          plan.Password.ValueStringPointer(),
			UserData:          plan.UserData.ValueStringPointer(),
			ClusterId:         optID(path.Root("cluster_id"), plan.ClusterID, d),
			StoragePoolId:     optID(path.Root("storage_pool_id"), plan.StoragePoolID, d),
			Migratable:        plan.Migratable.ValueBoolPointer(),
		}
		if !plan.Firmware.IsNull() && !plan.Firmware.IsUnknown() {
			f := client.Firmware(plan.Firmware.ValueString())
			body.Firmware = &f
		}
		if d.HasError() {
			return
		}
		res, err := r.data.api.CreateVmWithResponse(ctx, body)
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err != nil {
			apiError(d, "create the VM", err)
			return
		}
		accepted = res.JSON202
	}
	// Recorded at once, so a failure below leaves it in the state to
	// clean up.
	plan.ID = idValue(accepted.Id)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), plan.ID)...)
	if err := r.data.api.Wait(ctx, accepted.TaskId); err != nil {
		apiError(d, "create the VM", err)
		return
	}
	if start {
		if err := r.waitPower(ctx, accepted.Id, "running", 10*time.Minute); err != nil {
			apiError(d, "start the VM", err)
			return
		}
	}
	if plan.Template.ValueBool() {
		if err := r.patch(ctx, accepted.Id, client.UpdateVm{Template: plan.Template.ValueBoolPointer()}); err != nil {
			apiError(d, "make the VM a template", err)
			return
		}
	}
	v, err := r.get(ctx, accepted.Id)
	if err != nil {
		apiError(d, "read the VM", err)
		return
	}
	plan.set(ctx, v, d)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *vmResource) patch(ctx context.Context, id uuid.UUID, body client.UpdateVm) error {
	res, err := r.data.api.UpdateVmWithResponse(ctx, id, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	return err
}

func (r *vmResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state vmModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	v, err := r.get(ctx, id)
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the VM", err)
		return
	}
	state.set(ctx, v, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *vmResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	ctx, cancel := context.WithTimeout(ctx, vmTimeout)
	defer cancel()
	var plan, state vmModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	d := &resp.Diagnostics
	id := parseID(path.Root("id"), state.ID, d)
	if d.HasError() {
		return
	}

	// Security groups apply at once.
	if !plan.SecurityGroupIDs.IsUnknown() && !plan.SecurityGroupIDs.Equal(state.SecurityGroupIDs) {
		groups := idSet(ctx, plan.SecurityGroupIDs, d)
		nic := parseID(path.Root("nic_id"), state.NicID, d)
		if d.HasError() {
			return
		}
		res, err := r.data.api.SetNicGroupsWithResponse(ctx, id, nic, client.NicGroups{SecurityGroupIds: *groups})
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
		}
		if err != nil {
			apiError(d, "change the security groups", err)
			return
		}
	}
	// The disk grows in place, running or not.
	if !plan.DiskGib.IsUnknown() && plan.DiskGib.ValueInt64() > state.DiskGib.ValueInt64() {
		res, err := r.data.api.ResizeDiskWithResponse(ctx, id, 0, client.ResizeDisk{SizeGib: plan.DiskGib.ValueInt64()})
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
		}
		if err != nil {
			apiError(d, "grow the disk", err)
			return
		}
	}
	// CPUs, memory, migration and templates change while it is stopped.
	var change client.UpdateVm
	if !plan.Vcpus.IsUnknown() && !plan.Vcpus.Equal(state.Vcpus) {
		change.Vcpus = plan.Vcpus.ValueInt32Pointer()
	}
	if !plan.MemoryMib.IsUnknown() && !plan.MemoryMib.Equal(state.MemoryMib) {
		change.MemoryMib = plan.MemoryMib.ValueInt64Pointer()
	}
	if !plan.Migratable.IsUnknown() && !plan.Migratable.Equal(state.Migratable) {
		change.Migratable = plan.Migratable.ValueBoolPointer()
	}
	if !plan.Template.Equal(state.Template) {
		change.Template = plan.Template.ValueBoolPointer()
	}
	running := state.Running.ValueBool()
	if change != (client.UpdateVm{}) {
		if running {
			if err := r.power(ctx, id, false); err != nil {
				apiError(d, "stop the VM for the change", err)
				return
			}
			running = false
		}
		if err := r.patch(ctx, id, change); err != nil {
			apiError(d, "change the VM", err)
			return
		}
	}
	if want := plan.Running.ValueBool(); want != running {
		if err := r.power(ctx, id, want); err != nil {
			apiError(d, "change the VM's power", err)
			return
		}
	}
	v, err := r.get(ctx, id)
	if err != nil {
		apiError(d, "read the VM", err)
		return
	}
	plan.set(ctx, v, d)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *vmResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	ctx, cancel := context.WithTimeout(ctx, vmTimeout)
	defer cancel()
	var state vmModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteVmWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted, http.StatusNotFound)
	}
	if err == nil && res.JSON202 != nil {
		err = r.data.api.Wait(ctx, res.JSON202.TaskId)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the VM", err)
	}
}

func (r *vmResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}
