-- Disks of their own: volumes get a name, and a state while they are being
-- made, attached, detached, moved or deleted on their own (not with a VM).
ALTER TABLE volumes
    ADD COLUMN name text CHECK (name IS NULL OR length(name) BETWEEN 1 AND 64),
    ADD COLUMN state text NOT NULL DEFAULT 'ready'
        CHECK (state IN ('creating', 'ready', 'attaching', 'detaching', 'moving', 'deleting', 'failed')),
    ADD COLUMN error text,
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now();

CREATE UNIQUE INDEX volumes_name ON volumes (tenant_id, lower(name)) WHERE name IS NOT NULL;
CREATE INDEX volumes_by_tenant ON volumes (tenant_id);

CREATE TRIGGER volumes_notify AFTER INSERT OR UPDATE OR DELETE ON volumes
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('disk');

-- What each host's agent can do (the commands it handles), for every
-- replica to see, not only the one holding its session.
ALTER TABLE hosts ADD COLUMN agent_capabilities text[] NOT NULL DEFAULT '{}';
