-- What a host reports about itself that would give it more than it was
-- accepted with (a new name or address, another OVN chassis, physical
-- networks it did not have, more CPUs or memory) waits here until a
-- provider administrator accepts it: a host taken over must not make itself
-- a gateway for other tenants' VPCs, or attract their VMs.
ALTER TABLE hosts ADD COLUMN pending_facts jsonb;
