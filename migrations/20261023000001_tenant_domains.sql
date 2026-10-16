-- Domain names bound to tenants. Signing in at one starts from its tenant:
-- the tenant people enter there is a path below it (or nothing, for the
-- tenant itself).
CREATE TABLE tenant_domains (
    domain     text PRIMARY KEY
               CHECK (length(domain) <= 253
                      AND domain ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$'),
    tenant_id  uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tenant_domains_tenant ON tenant_domains (tenant_id);

-- A change to a tenant's domains is a change to the tenant.
CREATE OR REPLACE FUNCTION mst_notify_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rec record;
    row_id uuid;
    tenant_path text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        rec := OLD;
    ELSE
        rec := NEW;
    END IF;
    IF TG_TABLE_NAME = 'migrations' THEN
        row_id := rec.vm_id;
    ELSIF TG_TABLE_NAME = 'tenant_domains' THEN
        row_id := rec.tenant_id;
    ELSE
        row_id := rec.id;
    END IF;
    IF TG_TABLE_NAME = 'tenants' THEN
        tenant_path := rec.path::text;
    ELSIF TG_TABLE_NAME IN ('hosts', 'clusters', 'storage_pools', 'external_networks') THEN
        tenant_path := NULL;
    ELSIF TG_TABLE_NAME = 'security_group_rules' THEN
        row_id := rec.security_group_id;
        SELECT t.path::text INTO tenant_path
        FROM security_groups g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = row_id;
    ELSIF TG_TABLE_NAME = 'migrations' THEN
        SELECT t.path::text INTO tenant_path FROM vms v JOIN tenants t ON t.id = v.tenant_id WHERE v.id = row_id;
    ELSE
        SELECT path::text INTO tenant_path FROM tenants WHERE tenants.id = rec.tenant_id;
    END IF;
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', row_id,
        'op', CASE WHEN TG_TABLE_NAME IN ('security_group_rules', 'migrations', 'tenant_domains') THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path
    )::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER tenant_domains_notify AFTER INSERT OR UPDATE OR DELETE ON tenant_domains
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('tenant');
