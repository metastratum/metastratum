-- M5: host failure handling.

-- How to power a host off when it stops responding, so its VMs can safely
-- start elsewhere: `redfish` (a BMC) or `libvirt` (hosts that are VMs, as in
-- the lab). Secrets in the config are sealed with the master key.
ALTER TABLE hosts ADD COLUMN fence_kind text CHECK (fence_kind IN ('redfish', 'libvirt'));
ALTER TABLE hosts ADD COLUMN fence_config jsonb NOT NULL DEFAULT '{}';

-- fenced: powered off after it failed; its VMs were moved. An operator puts
-- it back in service once repaired.
ALTER TABLE hosts DROP CONSTRAINT hosts_state_check;
ALTER TABLE hosts ADD CONSTRAINT hosts_state_check
    CHECK (state IN ('ready', 'maintenance', 'decommissioned', 'fenced'));

-- Restart a failed host's VMs (those on shared storage) on the cluster's
-- other hosts.
ALTER TABLE clusters ADD COLUMN ha_enabled boolean NOT NULL DEFAULT false;
