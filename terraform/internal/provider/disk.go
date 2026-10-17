package provider

import (
	"context"
	"fmt"
	"net/http"
	"strings"

	"github.com/google/uuid"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

type diskResource struct{ base }

func newDiskResource() resource.Resource { return &diskResource{} }

type diskModel struct {
	ID            types.String `tfsdk:"id"`
	TenantID      types.String `tfsdk:"tenant_id"`
	Name          types.String `tfsdk:"name"`
	SizeGib       types.Int64  `tfsdk:"size_gib"`
	StoragePoolID types.String `tfsdk:"storage_pool_id"`
	HostID        types.String `tfsdk:"host_id"`
}

func (r *diskResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_disk"
}

func (r *diskResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An empty disk of its own, to attach to VMs (`metastratum_disk_attachment`). " +
			"Changing its pool or host moves it there (copied with what it holds); it must be attached to no VM then.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute(),
			"tenant_id": tenantAttribute(),
			"name": schema.StringAttribute{
				Description: "Unique among the tenant's disks.",
				Required:    true,
			},
			"size_gib": schema.Int64Attribute{
				Description: "Disks only grow: a larger size grows it in place (also while attached and running).",
				Required:    true,
			},
			"storage_pool_id": schema.StringAttribute{
				Description: "Local storage by default.",
				Optional:    true,
				Computed:    true,
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.UseStateForUnknown(),
				},
			},
			"host_id": schema.StringAttribute{
				Description: "The host a disk on local storage is on (required there); none for shared storage.",
				Optional:    true,
				Computed:    true,
				PlanModifiers: []planmodifier.String{
					stringplanmodifier.UseStateForUnknown(),
				},
			},
		},
	}
}

func (m *diskModel) set(d *client.DiskView) {
	m.ID = idValue(d.Id)
	m.TenantID = idValue(d.TenantId)
	m.Name = optString(d.Name)
	m.SizeGib = types.Int64Value(d.SizeGib)
	m.StoragePoolID = idValue(d.StoragePoolId)
	m.HostID = optIDValue(d.HostId)
}

func (r *diskResource) get(ctx context.Context, id uuid.UUID) (*client.DiskView, error) {
	res, err := r.data.api.GetDiskWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		return nil, err
	}
	return res.JSON200, nil
}

func (r *diskResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan diskModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	body := client.CreateDisk{
		Name:          plan.Name.ValueString(),
		SizeGib:       plan.SizeGib.ValueInt64(),
		TenantId:      r.tenant(plan.TenantID, &resp.Diagnostics),
		StoragePoolId: optID(path.Root("storage_pool_id"), plan.StoragePoolID, &resp.Diagnostics),
		HostId:        optID(path.Root("host_id"), plan.HostID, &resp.Diagnostics),
	}
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.CreateDiskWithResponse(ctx, body)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusCreated)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "create the disk", err)
		return
	}
	plan.set(res.JSON201)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *diskResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state diskModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.get(ctx, id)
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the disk", err)
		return
	}
	state.set(d)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *diskResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state diskModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	// Somewhere else: moved there first.
	pool, host := plan.StoragePoolID, plan.HostID
	if (!pool.IsUnknown() && !pool.Equal(state.StoragePoolID)) || (!host.IsUnknown() && !host.IsNull() && !host.Equal(state.HostID)) {
		body := client.MoveDisk{
			StoragePoolId: optID(path.Root("storage_pool_id"), pool, &resp.Diagnostics),
			HostId:        optID(path.Root("host_id"), host, &resp.Diagnostics),
		}
		if resp.Diagnostics.HasError() {
			return
		}
		res, err := r.data.api.MoveDiskWithResponse(ctx, id, body)
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err == nil {
			err = r.data.api.Wait(ctx, res.JSON202.TaskId)
		}
		if err != nil {
			apiError(&resp.Diagnostics, "move the disk", err)
			return
		}
	}
	update := client.UpdateDisk{}
	if !plan.Name.Equal(state.Name) {
		name := plan.Name.ValueString()
		update.Name = &name
	}
	if !plan.SizeGib.Equal(state.SizeGib) {
		size := plan.SizeGib.ValueInt64()
		update.SizeGib = &size
	}
	if update.Name != nil || update.SizeGib != nil {
		res, err := r.data.api.UpdateDiskWithResponse(ctx, id, update)
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
		}
		if err != nil {
			apiError(&resp.Diagnostics, "change the disk", err)
			return
		}
	}
	d, err := r.get(ctx, id)
	if err != nil {
		apiError(&resp.Diagnostics, "read the disk", err)
		return
	}
	plan.set(d)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *diskResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state diskModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	id := parseID(path.Root("id"), state.ID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DeleteDiskWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusNoContent, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "delete the disk", err)
	}
}

func (r *diskResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importByID(ctx, req, resp)
}

type diskAttachmentResource struct{ base }

func newDiskAttachmentResource() resource.Resource { return &diskAttachmentResource{} }

type diskAttachmentModel struct {
	ID        types.String `tfsdk:"id"`
	VMID      types.String `tfsdk:"vm_id"`
	DiskID    types.String `tfsdk:"disk_id"`
	GuestPath types.String `tfsdk:"guest_path"`
}

func (r *diskAttachmentResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_disk_attachment"
}

func (r *diskAttachmentResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A disk attached to a VM of its tenant: plugged in while it runs, or from its next start. " +
			"A disk on another host's local storage moves to the VM's host first. Destroyed, it is detached (unmount it in the guest first).",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Description:   "`<vm_id>/<disk_id>`.",
				Computed:      true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
			"vm_id":   schema.StringAttribute{Required: true, PlanModifiers: replace},
			"disk_id": schema.StringAttribute{Required: true, PlanModifiers: replace},
			"guest_path": schema.StringAttribute{
				Description:   "Where the guest finds it (by the disk's ID), for VMs that plug disks in.",
				Computed:      true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
		},
	}
}

func (r *diskAttachmentResource) disk(ctx context.Context, id uuid.UUID) (*client.DiskView, error) {
	res, err := r.data.api.GetDiskWithResponse(ctx, id)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
	}
	if err != nil {
		return nil, err
	}
	return res.JSON200, nil
}

func (r *diskAttachmentResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan diskAttachmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	vm := parseID(path.Root("vm_id"), plan.VMID, &resp.Diagnostics)
	diskID := parseID(path.Root("disk_id"), plan.DiskID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.disk(ctx, diskID)
	var host *client.HostId
	if err == nil {
		var v *client.GetVmResponse
		v, err = r.data.api.GetVmWithResponse(ctx, vm)
		if err == nil {
			err = client.Check(v.StatusCode(), v.Body, http.StatusOK)
		}
		if err == nil {
			host = v.JSON200.HostId
		}
	}
	if err == nil && d.StoragePoolKind == "local" && host != nil && (d.HostId == nil || *d.HostId != *host) {
		// On another host's storage: moved to the VM's, then attached.
		res, e := r.data.api.MoveDiskWithResponse(ctx, diskID, client.MoveDisk{VmId: &vm})
		err = e
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusAccepted)
		}
		if err == nil {
			err = r.data.api.Wait(ctx, res.JSON202.TaskId)
		}
	} else if err == nil {
		res, e := r.data.api.AttachDiskWithResponse(ctx, vm, client.AttachDisk{DiskId: diskID})
		err = e
		if err == nil {
			err = client.Check(res.StatusCode(), res.Body, http.StatusOK)
		}
	}
	if err == nil {
		d, err = r.disk(ctx, diskID)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "attach the disk", err)
		return
	}
	plan.ID = types.StringValue(fmt.Sprintf("%s/%s", vm, diskID))
	plan.GuestPath = optString(d.GuestPath)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *diskAttachmentResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state diskAttachmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	vm := parseID(path.Root("vm_id"), state.VMID, &resp.Diagnostics)
	diskID := parseID(path.Root("disk_id"), state.DiskID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.disk(ctx, diskID)
	if client.IsNotFound(err) || (err == nil && (d.VmId == nil || *d.VmId != vm)) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "read the disk", err)
		return
	}
	state.GuestPath = optString(d.GuestPath)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

// Update has nothing to do: every attribute replaces the attachment.
func (r *diskAttachmentResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan diskAttachmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *diskAttachmentResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state diskAttachmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	vm := parseID(path.Root("vm_id"), state.VMID, &resp.Diagnostics)
	diskID := parseID(path.Root("disk_id"), state.DiskID, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	res, err := r.data.api.DetachDiskWithResponse(ctx, vm, diskID)
	if err == nil {
		err = client.Check(res.StatusCode(), res.Body, http.StatusOK, http.StatusNotFound)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "detach the disk", err)
	}
}

// ImportState takes `<vm_id>/<disk_id>`.
func (r *diskAttachmentResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	vm, disk, ok := strings.Cut(req.ID, "/")
	if !ok {
		resp.Diagnostics.AddError("Invalid import ID", "expected <vm_id>/<disk_id>")
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("vm_id"), vm)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("disk_id"), disk)...)
}
