-- M3: VPCs, external networks, floating IPs, security groups and guardrail
-- policies. OVN is programmed from these tables by the network reconciler.

-- Exclusion constraints over (uuid, cidr) need btree_gist (a trusted
-- extension: the database owner may create it).
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- Provider networks outside OVN (a VLAN or flat segment on each host's
-- bridge), reached through VPC routers.
CREATE TABLE external_networks (
    id                uuid PRIMARY KEY,
    name              text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    -- Name in the hosts' ovn-bridge-mappings.
    physical_network  text NOT NULL,
    cidr              cidr NOT NULL,
    gateway           inet NOT NULL,
    -- Addresses metastratum hands out: VPC gateways and floating IPs.
    pool_start        inet NOT NULL,
    pool_end          inet NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (family(cidr) = 4 AND gateway << cidr AND pool_start << cidr AND pool_end << cidr
           AND pool_start <= pool_end)
);

ALTER TABLE tenant_grants DROP CONSTRAINT tenant_grants_kind_check;
ALTER TABLE tenant_grants ADD CONSTRAINT tenant_grants_kind_check
    CHECK (kind IN ('cluster', 'storage_pool', 'external_network'));

-- A tenant's isolated routing domain: one OVN logical router.
CREATE TABLE vpcs (
    id                   uuid PRIMARY KEY,
    tenant_id            uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    name                 text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    -- Where traffic leaves the VPC (SNAT), if anywhere.
    external_network_id  uuid REFERENCES external_networks (id) ON DELETE RESTRICT,
    created_at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- Every existing network moves into its tenant's default VPC.
INSERT INTO vpcs (id, tenant_id, name)
SELECT DISTINCT ON (tenant_id) gen_random_uuid(), tenant_id, 'default' FROM networks;
ALTER TABLE networks ADD COLUMN vpc_id uuid REFERENCES vpcs (id) ON DELETE RESTRICT;
UPDATE networks n SET vpc_id = v.id FROM vpcs v WHERE v.tenant_id = n.tenant_id AND v.name = 'default';
ALTER TABLE networks ALTER COLUMN vpc_id SET NOT NULL;
ALTER TABLE networks ADD CONSTRAINT networks_name_check CHECK (length(name) BETWEEN 1 AND 64);
-- Routed together, so their ranges must not overlap.
ALTER TABLE networks ADD CONSTRAINT networks_no_overlap EXCLUDE USING gist (vpc_id WITH =, cidr inet_ops WITH &&);
-- A switch port's IP must belong to the switch's network; network deletion
-- is refused while ports remain (nics.network_id is RESTRICT).

-- Addresses on external networks: a VPC's SNAT address, or a floating IP.
CREATE TABLE external_ips (
    id                   uuid PRIMARY KEY,
    external_network_id  uuid NOT NULL REFERENCES external_networks (id) ON DELETE RESTRICT,
    ip                   inet NOT NULL,
    kind                 text NOT NULL CHECK (kind IN ('gateway', 'floating')),
    tenant_id            uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    -- The VPC a gateway address belongs to.
    vpc_id               uuid UNIQUE REFERENCES vpcs (id) ON DELETE CASCADE,
    -- The NIC a floating IP points at.
    nic_id               uuid UNIQUE REFERENCES nics (id) ON DELETE SET NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (external_network_id, ip),
    CHECK ((kind = 'gateway') = (vpc_id IS NOT NULL)),
    CHECK (kind = 'floating' OR nic_id IS NULL)
);
CREATE INDEX external_ips_by_tenant ON external_ips (tenant_id);

-- Stateful firewalls on NICs. A NIC in no group passes no IP traffic.
CREATE TABLE security_groups (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    name         text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    description  text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- What a security group allows (replies are allowed implicitly).
CREATE TABLE security_group_rules (
    id                 uuid PRIMARY KEY,
    security_group_id  uuid NOT NULL REFERENCES security_groups (id) ON DELETE CASCADE,
    direction          text NOT NULL CHECK (direction IN ('ingress', 'egress')),
    protocol           text NOT NULL CHECK (protocol IN ('any', 'tcp', 'udp', 'icmp')),
    port_min           integer CHECK (port_min BETWEEN 1 AND 65535),
    port_max           integer CHECK (port_max BETWEEN 1 AND 65535),
    -- The other end: an address range, or the members of a group.
    remote_cidr        cidr,
    remote_group_id    uuid REFERENCES security_groups (id) ON DELETE CASCADE,
    description        text NOT NULL DEFAULT '',
    created_at         timestamptz NOT NULL DEFAULT now(),
    CHECK ((remote_cidr IS NULL) <> (remote_group_id IS NULL)),
    CHECK (remote_cidr IS NULL OR family(remote_cidr) = 4),
    CHECK ((port_min IS NULL) = (port_max IS NULL) AND port_min <= port_max),
    CHECK (protocol IN ('tcp', 'udp') OR port_min IS NULL)
);
CREATE INDEX security_group_rules_by_group ON security_group_rules (security_group_id);

CREATE TABLE nic_security_groups (
    nic_id             uuid NOT NULL REFERENCES nics (id) ON DELETE CASCADE,
    security_group_id  uuid NOT NULL REFERENCES security_groups (id) ON DELETE RESTRICT,
    PRIMARY KEY (nic_id, security_group_id)
);
CREATE INDEX nic_security_groups_by_group ON nic_security_groups (security_group_id);

-- Each tenant with NICs gets a default group: everything out, and in from
-- other members; existing NICs join it, so traffic between a tenant's VMs
-- keeps flowing.
INSERT INTO security_groups (id, tenant_id, name, description)
SELECT gen_random_uuid(), t.id, 'default', 'Allows all outgoing traffic, and incoming traffic from other members'
FROM tenants t WHERE EXISTS (SELECT 1 FROM vms v WHERE v.tenant_id = t.id);
INSERT INTO security_group_rules (id, security_group_id, direction, protocol, remote_cidr)
SELECT gen_random_uuid(), id, 'egress', 'any', '0.0.0.0/0' FROM security_groups;
INSERT INTO security_group_rules (id, security_group_id, direction, protocol, remote_group_id)
SELECT gen_random_uuid(), id, 'ingress', 'any', id FROM security_groups;
INSERT INTO nic_security_groups (nic_id, security_group_id)
SELECT n.id, g.id FROM nics n JOIN vms v ON v.id = n.vm_id JOIN security_groups g ON g.tenant_id = v.tenant_id;

-- Guardrails: deny rules a tenant sets for every VM in its subtree. Security
-- groups below cannot override them.
CREATE TABLE network_policies (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    direction    text NOT NULL CHECK (direction IN ('ingress', 'egress')),
    protocol     text NOT NULL CHECK (protocol IN ('any', 'tcp', 'udp', 'icmp')),
    port_min     integer CHECK (port_min BETWEEN 1 AND 65535),
    port_max     integer CHECK (port_max BETWEEN 1 AND 65535),
    remote_cidr  cidr NOT NULL CHECK (family(remote_cidr) = 4),
    description  text NOT NULL DEFAULT '',
    created_by   uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    CHECK ((port_min IS NULL) = (port_max IS NULL) AND port_min <= port_max),
    CHECK (protocol IN ('tcp', 'udp') OR port_min IS NULL)
);
CREATE INDEX network_policies_by_tenant ON network_policies (tenant_id);

-- Physical networks a host's OVS bridges reach (from ovn-bridge-mappings);
-- gateways for an external network are placed on hosts that have it.
ALTER TABLE hosts ADD COLUMN physical_networks text[] NOT NULL DEFAULT '{}';

ALTER TABLE quotas DROP CONSTRAINT quotas_resource_check;
ALTER TABLE quotas ADD CONSTRAINT quotas_resource_check
    CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips'));

-- Live updates for the new tables (see the events migration). External
-- networks are provider infrastructure; rule changes are announced as
-- changes to their group.
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
    row_id := rec.id;
    IF TG_TABLE_NAME = 'tenants' THEN
        tenant_path := rec.path::text;
    ELSIF TG_TABLE_NAME IN ('hosts', 'clusters', 'storage_pools', 'external_networks') THEN
        tenant_path := NULL;
    ELSIF TG_TABLE_NAME = 'security_group_rules' THEN
        row_id := rec.security_group_id;
        SELECT t.path::text INTO tenant_path
        FROM security_groups g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = row_id;
    ELSE
        SELECT path::text INTO tenant_path FROM tenants WHERE tenants.id = rec.tenant_id;
    END IF;
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', row_id,
        'op', CASE WHEN TG_TABLE_NAME = 'security_group_rules' THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path
    )::text);
    RETURN NULL;
END
$$;
CREATE TRIGGER security_group_rules_notify AFTER INSERT OR UPDATE OR DELETE ON security_group_rules
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('security_group');
CREATE TRIGGER vpcs_notify AFTER INSERT OR UPDATE OR DELETE ON vpcs
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vpc');
CREATE TRIGGER networks_notify AFTER INSERT OR UPDATE OR DELETE ON networks
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('network');
CREATE TRIGGER external_ips_notify AFTER INSERT OR UPDATE OR DELETE ON external_ips
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('external_ip');
CREATE TRIGGER security_groups_notify AFTER INSERT OR UPDATE OR DELETE ON security_groups
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('security_group');
CREATE TRIGGER network_policies_notify AFTER INSERT OR UPDATE OR DELETE ON network_policies
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('network_policy');
CREATE TRIGGER external_networks_notify AFTER INSERT OR UPDATE OR DELETE ON external_networks
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('external_network');
