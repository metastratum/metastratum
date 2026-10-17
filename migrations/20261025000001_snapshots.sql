-- Snapshots: points in time of a VM's disks, kept in the disks themselves
-- (qcow2 internal snapshots, Ceph RBD snapshots). They go with their VM.

CREATE TABLE snapshots (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id),
    vm_id       uuid NOT NULL REFERENCES vms (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    -- `creating` and `deleting` while the hosts work on it, `reverting`
    -- while the VM's disks are put back to it.
    state       text NOT NULL CHECK (state IN ('creating', 'ready', 'reverting', 'deleting', 'failed')),
    -- The VM's spec when it was taken.
    spec        jsonb NOT NULL,
    -- Whether its file systems were consistent: the VM was stopped, or its
    -- guest agent froze them. Otherwise as after a crash.
    consistent  boolean,
    error       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (vm_id, name)
);
CREATE INDEX snapshots_tenant ON snapshots (tenant_id);

CREATE TRIGGER snapshots_notify AFTER INSERT OR UPDATE OR DELETE ON snapshots
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('snapshot');
