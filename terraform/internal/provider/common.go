package provider

import (
	"context"
	"fmt"

	"github.com/google/uuid"
	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/defaults"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/metastratum/metastratum/terraform/internal/client"
)

// base is embedded by every resource: it holds the provider's data.
type base struct {
	data *providerData
}

func (b *base) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	if req.ProviderData == nil {
		return
	}
	d, ok := req.ProviderData.(*providerData)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", fmt.Sprintf("%T", req.ProviderData))
		return
	}
	b.data = d
}

// dsBase is base for data sources.
type dsBase struct {
	data *providerData
}

func (b *dsBase) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	if req.ProviderData == nil {
		return
	}
	d, ok := req.ProviderData.(*providerData)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", fmt.Sprintf("%T", req.ProviderData))
		return
	}
	b.data = d
}

// importByID lets `terraform import` take a resource's ID.
func importByID(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}

// tenant is the tenant a resource names, or else the provider's default (nil
// for the token's own).
func (b *base) tenant(v types.String, diags *diag.Diagnostics) *client.TenantId {
	s := b.data.tenant
	if !v.IsNull() && !v.IsUnknown() {
		s = v.ValueString()
	}
	if s == "" {
		return nil
	}
	id := parseID(path.Root("tenant_id"), types.StringValue(s), diags)
	return &id
}

// parseID reads a UUID attribute.
func parseID(p path.Path, v types.String, diags *diag.Diagnostics) uuid.UUID {
	id, err := uuid.Parse(v.ValueString())
	if err != nil {
		diags.AddAttributeError(p, "Not an ID", fmt.Sprintf("%q is not a UUID", v.ValueString()))
	}
	return id
}

// optID reads an optional UUID attribute.
func optID(p path.Path, v types.String, diags *diag.Diagnostics) *uuid.UUID {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	id := parseID(p, v, diags)
	return &id
}

func idValue(id uuid.UUID) types.String {
	return types.StringValue(id.String())
}

func optIDValue(id *uuid.UUID) types.String {
	if id == nil {
		return types.StringNull()
	}
	return types.StringValue(id.String())
}

func optString(v *string) types.String {
	if v == nil {
		return types.StringNull()
	}
	return types.StringValue(*v)
}

// apiError reports a failed call. A missing thing during Read is not an
// error: the caller removes it from the state instead.
func apiError(diags *diag.Diagnostics, what string, err error) {
	diags.AddError(fmt.Sprintf("Could not %s", what), err.Error())
}

func parseUUID(s string) (uuid.UUID, error) {
	return uuid.Parse(s)
}

// stringDefault is a static default for an optional, computed string.
func stringDefault(v string) defaults.String {
	return stringdefault.StaticString(v)
}
