-- A chassis name belongs to one host: the controller signs OVN certificates
-- for the chassis a host reports, and OVN's RBAC trusts that name.
CREATE UNIQUE INDEX hosts_ovn_chassis ON hosts (ovn_chassis)
    WHERE ovn_chassis <> '' AND state <> 'decommissioned';
