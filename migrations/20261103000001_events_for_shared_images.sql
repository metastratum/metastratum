-- Changes to an image reach users of tenants below its owner only while
-- it is shared with them: the notification says whether it is.
CREATE OR REPLACE FUNCTION mst_notify_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rec record;
    row_id uuid;
    tenant_path text;
    shared boolean;
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
    ELSIF TG_TABLE_NAME IN ('hosts', 'clusters', 'storage_pools', 'external_networks', 'backup_repositories') THEN
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
    -- Images are seen below their tenant only while shared: say whether
    -- this one is (or was, as it stops being).
    IF TG_TABLE_NAME = 'images' THEN
        shared := rec.shared OR (TG_OP = 'UPDATE' AND OLD.shared);
    END IF;
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', row_id,
        'op', CASE WHEN TG_TABLE_NAME IN ('security_group_rules', 'migrations', 'tenant_domains') THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path,
        'shared', shared
    )::text);
    RETURN NULL;
END
$$;
