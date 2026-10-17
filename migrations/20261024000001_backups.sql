-- Backups: copies of VMs' disks in backup repositories, apart from the
-- storage the VMs run on. They outlive their VM.

-- Where backups are kept: NFS exports the hosts mount, set up by the
-- provider and granted to tenants like storage pools.
CREATE TABLE backup_repositories (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    kind        text NOT NULL CHECK (kind IN ('nfs')),
    config      jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE backups (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id),
    -- The VM it was taken of, while it exists; its name and spec stay.
    vm_id          uuid REFERENCES vms (id) ON DELETE SET NULL,
    vm_name        text NOT NULL,
    repository_id  uuid NOT NULL REFERENCES backup_repositories (id),
    state          text NOT NULL CHECK (state IN ('running', 'ready', 'failed')),
    -- Taken by the VM's schedule (and pruned by it), or by hand.
    scheduled      boolean NOT NULL DEFAULT false,
    -- The VM's spec when it was taken, for restoring it.
    spec           jsonb NOT NULL,
    -- The disks' virtual sizes added up: what it counts against quotas.
    size_bytes     bigint NOT NULL,
    -- What it takes up in the repository, once taken.
    stored_bytes   bigint,
    -- Whether its file systems were consistent: the VM was stopped, or its
    -- guest agent froze them for the start. Otherwise as after a crash.
    consistent     boolean,
    error          text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    completed_at   timestamptz
);
CREATE INDEX backups_vm ON backups (vm_id, created_at);
CREATE INDEX backups_tenant ON backups (tenant_id, created_at);

-- A VM's backup schedule: how often, how many scheduled backups to keep,
-- and where.
ALTER TABLE vms ADD COLUMN backup_schedule text CHECK (backup_schedule IN ('daily', 'weekly'));
ALTER TABLE vms ADD COLUMN backup_keep integer CHECK (backup_keep BETWEEN 1 AND 365);
ALTER TABLE vms ADD COLUMN backup_repository_id uuid REFERENCES backup_repositories (id);

ALTER TABLE tenant_grants DROP CONSTRAINT tenant_grants_kind_check;
ALTER TABLE tenant_grants ADD CONSTRAINT tenant_grants_kind_check
    CHECK (kind IN ('cluster', 'storage_pool', 'external_network', 'backup_repository'));

ALTER TABLE quotas DROP CONSTRAINT quotas_resource_check;
ALTER TABLE quotas ADD CONSTRAINT quotas_resource_check
    CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib'));

-- Live updates: backups are the tenant's, repositories infrastructure.
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
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', row_id,
        'op', CASE WHEN TG_TABLE_NAME IN ('security_group_rules', 'migrations', 'tenant_domains') THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path
    )::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER backups_notify AFTER INSERT OR UPDATE OR DELETE ON backups
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('backup');
CREATE TRIGGER backup_repositories_notify AFTER INSERT OR UPDATE OR DELETE ON backup_repositories
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('backup_repository');
