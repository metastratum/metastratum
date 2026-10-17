-- A guardrail belongs to the tenant whose administrators set it, which may
-- be above the tenant it applies to: only they remove it. (Before, the
-- tenant it applied to owned it, so a sub-tenant's administrators could
-- remove what a tenant above had set on them.) Existing ones: the highest
-- tenant their creator administered them from, else where they apply.
ALTER TABLE network_policies
    ADD COLUMN owner_tenant_id uuid REFERENCES tenants (id) ON DELETE CASCADE;
UPDATE network_policies p SET owner_tenant_id = coalesce((
    SELECT t.id FROM role_bindings b JOIN tenants t ON t.id = b.tenant_id
    WHERE b.user_id = p.created_by AND b.role = 'admin'
      AND t.path @> (SELECT path FROM tenants WHERE id = p.tenant_id)
    ORDER BY nlevel(t.path) LIMIT 1), p.tenant_id);
ALTER TABLE network_policies ALTER COLUMN owner_tenant_id SET NOT NULL;
