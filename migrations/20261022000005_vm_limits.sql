-- Limits for each VM in the cluster, set by the provider so VMs sharing
-- hosts and storage cannot starve each other. NULL: none.
ALTER TABLE clusters
    ADD COLUMN vm_net_mbps integer CHECK (vm_net_mbps > 0),   -- each way, per NIC
    ADD COLUMN vm_disk_iops integer CHECK (vm_disk_iops > 0), -- per disk
    ADD COLUMN vm_disk_mbps integer CHECK (vm_disk_mbps > 0); -- per disk, MB/s
