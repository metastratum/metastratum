-- Tenants' networks stay out of external networks' ranges. A network
-- overlapping one would let a tenant hand out (to a VM, or an internal load
-- balancer) another tenant's floating IP, or the external gateway's
-- address, on its own router, which sits on that external network.
-- Checked from both sides, one at a time.

CREATE FUNCTION mst_network_outside_external() RETURNS trigger AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtext('mst_network_ranges'));
    IF TG_TABLE_NAME = 'networks' THEN
        IF EXISTS (SELECT 1 FROM external_networks e WHERE e.cidr && NEW.cidr) THEN
            RAISE EXCEPTION 'network range % overlaps an external network', NEW.cidr
                USING ERRCODE = 'exclusion_violation', CONSTRAINT = 'networks_outside_external';
        END IF;
    ELSIF EXISTS (SELECT 1 FROM networks n WHERE n.cidr && NEW.cidr) THEN
        RAISE EXCEPTION 'external range % overlaps a tenant network', NEW.cidr
            USING ERRCODE = 'exclusion_violation', CONSTRAINT = 'networks_outside_external';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER networks_outside_external BEFORE INSERT OR UPDATE OF cidr ON networks
    FOR EACH ROW EXECUTE FUNCTION mst_network_outside_external();
CREATE TRIGGER external_networks_outside_networks BEFORE INSERT OR UPDATE OF cidr ON external_networks
    FOR EACH ROW EXECUTE FUNCTION mst_network_outside_external();
