-- M2: clusters of hosts.

-- A cluster is a group of hosts that VMs are placed (and later migrate)
-- within. Overcommit ratios scale hosts' physical CPUs and memory into what
-- the scheduler may allocate.
CREATE TABLE clusters (
    id                 uuid PRIMARY KEY,
    name               text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    cpu_overcommit     real NOT NULL DEFAULT 4.0 CHECK (cpu_overcommit > 0),
    memory_overcommit  real NOT NULL DEFAULT 1.0 CHECK (memory_overcommit > 0),
    created_at         timestamptz NOT NULL DEFAULT now()
);

-- Hosts outside any cluster are not scheduled on.
ALTER TABLE hosts ADD COLUMN cluster_id uuid REFERENCES clusters (id) ON DELETE RESTRICT;
CREATE INDEX hosts_by_cluster ON hosts (cluster_id);

-- Existing hosts form the default cluster.
INSERT INTO clusters (id, name) VALUES (gen_random_uuid(), 'default');
UPDATE hosts SET cluster_id = (SELECT id FROM clusters WHERE name = 'default');

-- Hosts enrolling with a token join the token's cluster.
ALTER TABLE join_tokens ADD COLUMN cluster_id uuid REFERENCES clusters (id) ON DELETE CASCADE;
