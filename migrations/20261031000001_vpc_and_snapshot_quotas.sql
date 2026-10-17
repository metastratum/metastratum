-- Quotas for what had none: VPCs (each connected to an external network
-- takes an address from its pool, which one tenant could otherwise empty)
-- and snapshots (which keep every block overwritten since: up to 16 per VM,
-- each charged the VM's disk size when taken, as what they grow to cannot
-- be known in advance).

ALTER TABLE snapshots ADD COLUMN size_bytes bigint NOT NULL DEFAULT 0;
UPDATE snapshots s
SET size_bytes = coalesce((SELECT sum(v.size_bytes) FROM volumes v WHERE v.vm_id = s.vm_id), 0);

ALTER TABLE quotas DROP CONSTRAINT quotas_resource_check;
ALTER TABLE quotas ADD CONSTRAINT quotas_resource_check
    CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib',
                        'load_balancers', 'vpcs', 'snapshot_gib'));
