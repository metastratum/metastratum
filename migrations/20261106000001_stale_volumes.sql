-- Copies of disks left where the disks no longer are: a move's old copy,
-- or what a failed move copied, removed in the background.
CREATE TABLE stale_volumes (
    id          uuid PRIMARY KEY,
    volume_id   uuid NOT NULL,
    pool_id     uuid NOT NULL REFERENCES storage_pools (id) ON DELETE CASCADE,
    -- For local storage: the host it is on.
    host_id     uuid REFERENCES hosts (id) ON DELETE CASCADE,
    attempts    int NOT NULL DEFAULT 0,
    last_error  text,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- A disk attached to no VM being grown.
ALTER TABLE volumes DROP CONSTRAINT volumes_state_check;
ALTER TABLE volumes ADD CONSTRAINT volumes_state_check
    CHECK (state IN ('creating', 'ready', 'attaching', 'detaching', 'moving', 'growing', 'deleting', 'failed'));
