-- M2: storage pools.

-- Where volumes live. `local` is each host's own disk and is built in; shared
-- pools (NFS, Ceph RBD) are reachable from the clusters they are attached
-- to. `config` holds the pool's connection details; secrets in it (an RBD
-- key) are never returned by the API.
-- TODO(M5): encrypt secrets at rest.
CREATE TABLE storage_pools (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    kind        text NOT NULL CHECK (kind IN ('local', 'nfs', 'rbd')),
    config      jsonb NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- The built-in local pool has a fixed ID (hm_core::ids::LOCAL_POOL).
INSERT INTO storage_pools (id, name, kind) VALUES ('00000000-0000-0000-0000-000000000001', 'local', 'local');

CREATE TABLE storage_pool_clusters (
    pool_id     uuid NOT NULL REFERENCES storage_pools (id) ON DELETE CASCADE,
    cluster_id  uuid NOT NULL REFERENCES clusters (id) ON DELETE CASCADE,
    PRIMARY KEY (pool_id, cluster_id)
);

-- Volumes reference their pool instead of naming it.
ALTER TABLE volumes ADD COLUMN pool_id uuid REFERENCES storage_pools (id) ON DELETE RESTRICT;
UPDATE volumes SET pool_id = '00000000-0000-0000-0000-000000000001';
ALTER TABLE volumes ALTER COLUMN pool_id SET NOT NULL;
ALTER TABLE volumes DROP COLUMN pool;
