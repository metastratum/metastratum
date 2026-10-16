-- M4: live migration.

-- What hosts report for migration: their host-model CPU (for cluster
-- baselines) and whether libvirt accepts TLS connections from other hosts.
ALTER TABLE hosts ADD COLUMN cpu_json text NOT NULL DEFAULT '';
ALTER TABLE hosts ADD COLUMN migration_ready boolean NOT NULL DEFAULT false;

-- A migrating VM runs on both hosts for a while; its status says so, and
-- what either host reports about it is taken with that in mind.
ALTER TABLE vms DROP CONSTRAINT vms_status_check;
ALTER TABLE vms ADD CONSTRAINT vms_status_check
    CHECK (status IN ('creating', 'ready', 'error', 'deleting', 'migrating'));

-- Migrations in progress, one per VM. The row reserves the VM's CPU and
-- memory on the destination until the VM moves there or the migration is
-- rolled back.
CREATE TABLE migrations (
    vm_id             uuid PRIMARY KEY REFERENCES vms (id) ON DELETE CASCADE,
    task_id           uuid NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    source_host_id    uuid NOT NULL REFERENCES hosts (id),
    dest_host_id      uuid NOT NULL REFERENCES hosts (id),
    -- preparing: setting up the destination; running: libvirt is migrating;
    -- committing: the VM runs on the destination, cleanup remains.
    state             text NOT NULL DEFAULT 'preparing'
                      CHECK (state IN ('preparing', 'running', 'committing')),
    vcpus             integer NOT NULL,
    memory_mib        bigint NOT NULL,
    cancel_requested  boolean NOT NULL DEFAULT false,
    started_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_host_id <> dest_host_id)
);
CREATE INDEX migrations_by_dest ON migrations (dest_host_id);

-- A migration is announced as a change to its VM.
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
        'op', CASE WHEN TG_TABLE_NAME IN ('security_group_rules', 'migrations') THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path
    )::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER migrations_notify AFTER INSERT OR UPDATE OR DELETE ON migrations
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vm');
