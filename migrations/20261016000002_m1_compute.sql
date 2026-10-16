-- M1: hosts and enrollment, sessions, images, networks, VMs and consoles.

-- Key material owned by the control plane (the internal CA). Shared through
-- the database so every controller replica uses the same CA.
-- TODO(M5): encrypt private keys at rest.
CREATE TABLE pki (
    name        text PRIMARY KEY,
    cert_pem    text NOT NULL,
    key_pem     text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE hosts (
    id                uuid PRIMARY KEY,
    name              text NOT NULL UNIQUE,
    address           inet NOT NULL,
    state             text NOT NULL DEFAULT 'ready'
                      CHECK (state IN ('ready', 'maintenance', 'decommissioned')),
    cpu_count         integer NOT NULL,
    memory_mib        bigint NOT NULL,
    cpu_model         text NOT NULL DEFAULT '',
    kvm               boolean NOT NULL DEFAULT false,
    libvirt_version   text NOT NULL DEFAULT '',
    ovn_chassis       text NOT NULL DEFAULT '',
    agent_version     text NOT NULL DEFAULT '',
    cert_fingerprint  text NOT NULL,
    cert_expires_at   timestamptz NOT NULL,
    -- Which controller replica holds the agent's session, if any.
    connected_to      text,
    last_seen_at      timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE join_tokens (
    id           text PRIMARY KEY,
    secret_hash  text NOT NULL,
    created_by   uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz,
    host_id      uuid REFERENCES hosts (id) ON DELETE SET NULL
);

-- Browser sessions. Only a hash of the session secret is stored.
CREATE TABLE sessions (
    id_hash       text PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_expiry ON sessions (expires_at);

CREATE TABLE images (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    name        text NOT NULL,
    source_url  text NOT NULL,
    format      text NOT NULL CHECK (format IN ('qcow2', 'raw')),
    -- Visible to the owning tenant's whole subtree, not just the owner.
    shared      boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE TABLE networks (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    name        text NOT NULL,
    cidr        cidr NOT NULL,
    gateway     inet NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name),
    CHECK (family(cidr) = 4 AND gateway << cidr)
);

CREATE TABLE vms (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    name           text NOT NULL CHECK (name ~ '^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$'),
    host_id        uuid REFERENCES hosts (id) ON DELETE RESTRICT,
    spec           jsonb NOT NULL,
    generation     bigint NOT NULL DEFAULT 1,
    status         text NOT NULL DEFAULT 'creating'
                   CHECK (status IN ('creating', 'ready', 'error', 'deleting')),
    desired_power  text NOT NULL DEFAULT 'running' CHECK (desired_power IN ('running', 'stopped')),
    -- Last power state reported by the host agent.
    power          text NOT NULL DEFAULT 'unknown',
    error          text,
    created_by     uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX vms_by_host ON vms (host_id);

CREATE TABLE volumes (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    vm_id       uuid REFERENCES vms (id) ON DELETE SET NULL,
    -- Set for host-local pools; the volume only exists on that host.
    host_id     uuid REFERENCES hosts (id) ON DELETE RESTRICT,
    pool        text NOT NULL DEFAULT 'local',
    size_bytes  bigint NOT NULL CHECK (size_bytes > 0),
    -- Volumes are copy-on-write overlays of the image, so it must outlive them.
    image_id    uuid REFERENCES images (id) ON DELETE RESTRICT,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX volumes_by_vm ON volumes (vm_id);

CREATE TABLE nics (
    id          uuid PRIMARY KEY,
    vm_id       uuid NOT NULL REFERENCES vms (id) ON DELETE CASCADE,
    network_id  uuid NOT NULL REFERENCES networks (id) ON DELETE RESTRICT,
    mac         text NOT NULL,
    ip          inet NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (network_id, ip),
    UNIQUE (network_id, mac)
);

-- Single-use, short-lived console tickets. Stored in the database so the
-- WebSocket may land on any controller replica.
CREATE TABLE console_tickets (
    token_hash  text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    vm_id       uuid NOT NULL REFERENCES vms (id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('vnc', 'serial')),
    expires_at  timestamptz NOT NULL
);
