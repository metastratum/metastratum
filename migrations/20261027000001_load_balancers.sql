-- Load balancers: a virtual IP whose listeners spread connections over
-- member VMs, by OVN in the VPC's router and switches.

-- Addresses in tenant networks held by something other than a NIC: a load
-- balancer's internal virtual IP, or the address OVN's health checks come
-- from (one per network). NIC allocation skips them.
CREATE TABLE network_addresses (
    network_id  uuid NOT NULL REFERENCES networks (id) ON DELETE CASCADE,
    ip          inet NOT NULL,
    purpose     text NOT NULL CHECK (purpose IN ('lb-vip', 'lb-monitor')),
    PRIMARY KEY (network_id, ip)
);
CREATE UNIQUE INDEX network_addresses_monitor ON network_addresses (network_id)
    WHERE purpose = 'lb-monitor';

ALTER TABLE external_ips DROP CONSTRAINT external_ips_kind_check;
ALTER TABLE external_ips ADD CONSTRAINT external_ips_kind_check
    CHECK (kind IN ('gateway', 'floating', 'load_balancer'));

CREATE TABLE load_balancers (
    id              uuid PRIMARY KEY,
    tenant_id       uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    vpc_id          uuid NOT NULL REFERENCES vpcs (id) ON DELETE RESTRICT,
    name            text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    -- The virtual IP: on the VPC's external network, or in one of its
    -- networks.
    external_ip_id  uuid UNIQUE REFERENCES external_ips (id) ON DELETE RESTRICT,
    network_id      uuid REFERENCES networks (id) ON DELETE RESTRICT,
    ip              inet NOT NULL,
    -- OVN checks members (TCP connects, UDP probes) and leaves out those
    -- that do not answer.
    health_check    boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name),
    CHECK ((external_ip_id IS NULL) <> (network_id IS NULL))
);

CREATE TABLE load_balancer_listeners (
    load_balancer_id  uuid NOT NULL REFERENCES load_balancers (id) ON DELETE CASCADE,
    protocol          text NOT NULL CHECK (protocol IN ('tcp', 'udp')),
    port              integer NOT NULL CHECK (port BETWEEN 1 AND 65535),
    -- The members' port.
    target_port       integer NOT NULL CHECK (target_port BETWEEN 1 AND 65535),
    PRIMARY KEY (load_balancer_id, protocol, port)
);

CREATE TABLE load_balancer_members (
    load_balancer_id  uuid NOT NULL REFERENCES load_balancers (id) ON DELETE CASCADE,
    nic_id            uuid NOT NULL REFERENCES nics (id) ON DELETE CASCADE,
    PRIMARY KEY (load_balancer_id, nic_id)
);
CREATE INDEX load_balancer_members_nic ON load_balancer_members (nic_id);

ALTER TABLE quotas DROP CONSTRAINT quotas_resource_check;
ALTER TABLE quotas ADD CONSTRAINT quotas_resource_check
    CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib',
                        'load_balancers'));

CREATE TRIGGER load_balancers_notify AFTER INSERT OR UPDATE OR DELETE ON load_balancers
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('load_balancer');
