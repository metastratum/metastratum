-- Change notifications for live updates. Every change to a watched table is
-- announced on the `mst_events` channel, so each controller replica can tell
-- its browsers what to refresh. Notifications carry what the row is and which
-- tenant owns it (for authorization), never its contents.

CREATE FUNCTION mst_notify_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rec record;
    tenant_path text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        rec := OLD;
    ELSE
        rec := NEW;
    END IF;
    IF TG_TABLE_NAME = 'tenants' THEN
        tenant_path := rec.path::text;
    ELSIF TG_TABLE_NAME IN ('hosts', 'clusters', 'storage_pools') THEN
        -- Provider infrastructure: no owning tenant.
        tenant_path := NULL;
    ELSE
        SELECT path::text INTO tenant_path FROM tenants WHERE id = rec.tenant_id;
    END IF;
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', rec.id,
        'op', lower(TG_OP),
        'tenant', tenant_path
    )::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER vms_notify AFTER INSERT OR UPDATE OR DELETE ON vms
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vm');
CREATE TRIGGER images_notify AFTER INSERT OR UPDATE OR DELETE ON images
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('image');
CREATE TRIGGER tenants_notify AFTER INSERT OR UPDATE OR DELETE ON tenants
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('tenant');
CREATE TRIGGER clusters_notify AFTER INSERT OR UPDATE OR DELETE ON clusters
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('cluster');
CREATE TRIGGER storage_pools_notify AFTER INSERT OR UPDATE OR DELETE ON storage_pools
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('storage_pool');

-- Tasks renew their lease and hosts heartbeat every few seconds; only changes
-- someone would see are announced.
CREATE TRIGGER tasks_notify AFTER INSERT OR DELETE ON tasks
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('task');
CREATE TRIGGER tasks_notify_update AFTER UPDATE ON tasks
    FOR EACH ROW
    WHEN ((OLD.state, OLD.step, OLD.progress, OLD.error) IS DISTINCT FROM (NEW.state, NEW.step, NEW.progress, NEW.error))
    EXECUTE FUNCTION mst_notify_change('task');

CREATE TRIGGER hosts_notify AFTER INSERT OR DELETE ON hosts
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('host');
CREATE TRIGGER hosts_notify_update AFTER UPDATE ON hosts
    FOR EACH ROW
    WHEN ((OLD.name, OLD.address, OLD.state, OLD.cluster_id, OLD.connected_to, OLD.cpu_count, OLD.memory_mib,
           OLD.cpu_model, OLD.kvm, OLD.libvirt_version, OLD.ovn_chassis, OLD.agent_version, OLD.cert_expires_at)
          IS DISTINCT FROM
          (NEW.name, NEW.address, NEW.state, NEW.cluster_id, NEW.connected_to, NEW.cpu_count, NEW.memory_mib,
           NEW.cpu_model, NEW.kvm, NEW.libvirt_version, NEW.ovn_chassis, NEW.agent_version, NEW.cert_expires_at))
    EXECUTE FUNCTION mst_notify_change('host');
