-- metastratum's schema, from 0.5 on (earlier installations are not
-- upgraded: they are installed afresh). Later releases add migrations of
-- their own after this one.

CREATE EXTENSION IF NOT EXISTS ltree;
-- Exclusion constraints over (uuid, cidr) need btree_gist (a trusted
-- extension: the database owner may create it).
CREATE EXTENSION IF NOT EXISTS btree_gist;


-- ===================================================================
-- Tenants, people and access
-- ===================================================================

-- Tenants form a tree rooted at the provider. `path` is the materialised
-- lineage (root first) whose labels are tenant IDs in simple hex form; it is
-- maintained by trigger so callers only ever supply `parent_id`.
CREATE TABLE tenants (
    id                  uuid PRIMARY KEY,
    parent_id           uuid REFERENCES tenants (id) ON DELETE RESTRICT,
    slug                text NOT NULL CHECK (slug ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    name                text NOT NULL,
    path                ltree NOT NULL UNIQUE,
    -- Password accounts of the tenant and everything below it must use a
    -- second factor; those without one can only add one.
    require_two_factor  boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- Slugs are unique among siblings; the root has no siblings.
CREATE UNIQUE INDEX tenants_sibling_slug ON tenants (coalesce(parent_id, '00000000-0000-0000-0000-000000000000'::uuid), slug);
-- Exactly one root tenant.
CREATE UNIQUE INDEX tenants_single_root ON tenants ((true)) WHERE parent_id IS NULL;
CREATE INDEX tenants_path_gist ON tenants USING gist (path);

CREATE FUNCTION tenants_set_path() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    parent_path ltree;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.parent_id IS DISTINCT FROM OLD.parent_id THEN
        RAISE EXCEPTION 'reparenting tenants is not supported';
    END IF;
    IF NEW.parent_id IS NULL THEN
        NEW.path := text2ltree(replace(NEW.id::text, '-', ''));
    ELSE
        SELECT path INTO parent_path FROM tenants WHERE id = NEW.parent_id;
        IF parent_path IS NULL THEN
            RAISE EXCEPTION 'parent tenant % does not exist', NEW.parent_id;
        END IF;
        NEW.path := parent_path || text2ltree(replace(NEW.id::text, '-', ''));
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER tenants_set_path
    BEFORE INSERT OR UPDATE OF parent_id ON tenants
    FOR EACH ROW EXECUTE FUNCTION tenants_set_path();

-- Domain names bound to tenants. Signing in at one starts from its tenant:
-- the tenant people enter there is a path below it (or nothing, for the
-- tenant itself).
CREATE TABLE tenant_domains (
    domain     text PRIMARY KEY
               CHECK (length(domain) <= 253
                      AND domain ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$'),
    tenant_id  uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tenant_domains_tenant ON tenant_domains (tenant_id);

-- Users belong to a home tenant. Local users have a password hash; users that
-- come from a tenant's OIDC provider do not, and are matched by its stable
-- subject ID.
CREATE TABLE users (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    username          text NOT NULL CHECK (username ~ '^[A-Za-z0-9._@-]{1,128}$'),
    display_name      text NOT NULL DEFAULT '',
    email             text,
    password_hash     text,
    disabled          boolean NOT NULL DEFAULT false,
    external_subject  text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, username)
);
CREATE UNIQUE INDEX users_external_subject ON users (tenant_id, external_subject) WHERE external_subject IS NOT NULL;

-- A user holds a role at a tenant; it applies to that tenant and every tenant
-- below it. Roles are built in (see mst-authz). Bindings an identity
-- provider's groups produced (`oidc`) are re-synced at every sign-in;
-- manual ones are left alone.
CREATE TABLE role_bindings (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    role        text NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    source      text NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'oidc')),
    UNIQUE (user_id, tenant_id, role)
);
CREATE INDEX role_bindings_by_tenant ON role_bindings (tenant_id);

-- People hold roles only at their own tenant or below it. Their account is
-- managed there (password, second factors, deletion, identity provider), so
-- a role higher up or beside it would let that tenant's administrators take
-- over a role they do not hold themselves.
CREATE FUNCTION mst_binding_in_home_subtree() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM users u
        JOIN tenants home ON home.id = u.tenant_id
        JOIN tenants t ON t.id = NEW.tenant_id
        WHERE u.id = NEW.user_id AND home.path @> t.path
    ) THEN
        RAISE EXCEPTION 'a role must be at the user''s own tenant or below it'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER role_bindings_in_home_subtree BEFORE INSERT OR UPDATE ON role_bindings
    FOR EACH ROW EXECUTE FUNCTION mst_binding_in_home_subtree();

-- Long-lived credentials for automation. They act as their user, with that
-- user's role bindings. Only a hash of the secret is stored.
CREATE TABLE api_tokens (
    id            uuid PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name          text NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    token_hash    text NOT NULL UNIQUE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz,
    last_used_at  timestamptz,
    -- What the token may do, as roles at tenants ({"tenant_id", "role"}),
    -- on top of what its user may do. NULL: everything its user may.
    scopes        jsonb
);
CREATE INDEX api_tokens_by_user ON api_tokens (user_id);

-- Browser sessions. Only a hash of the session secret is stored.
CREATE TABLE sessions (
    id_hash       text PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_expiry ON sessions (expires_at);

-- Two-factor sign-in for password accounts: authenticator apps (TOTP),
-- security keys (WebAuthn, which also speaks to FIDO U2F keys), YubiKeys
-- typing Yubico OTPs, and backup codes.

-- A user's second factors. What each holds depends on its kind:
--   totp:       {"secret": sealed base32 secret, "last_step": last time step used}
--   webauthn:   {"credential_id", "public_key" (COSE): base64url, "rp_id",
--                "sign_count", "transports": [..]}
--   yubico_otp: {"public_id": the key's 12 modhex characters}
CREATE TABLE second_factors (
    id            uuid PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind          text NOT NULL CHECK (kind IN ('totp', 'webauthn', 'yubico_otp')),
    name          text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    data          jsonb NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz
);
CREATE INDEX second_factors_user ON second_factors (user_id);

-- Single-use backup codes, hashed with the user's ID.
CREATE TABLE backup_codes (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  text NOT NULL,
    used_at    timestamptz,
    PRIMARY KEY (user_id, code_hash)
);

-- Half-finished sign-ins (the password was right, the second factor is to
-- come) and enrollments (a security key's or authenticator app's first
-- answer is to come). Keyed by a hash of the secret the client holds.
CREATE TABLE auth_challenges (
    id_hash     text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose     text NOT NULL CHECK (purpose IN ('login', 'webauthn', 'totp')),
    -- login: {"challenge", "rp_id", "origin"}; webauthn: {"challenge",
    -- "rp_id", "origin"}; totp: {"secret": sealed}.
    data        jsonb NOT NULL,
    attempts    integer NOT NULL DEFAULT 0,
    expires_at  timestamptz NOT NULL
);
CREATE INDEX auth_challenges_expiry ON auth_challenges (expires_at);

-- A tenant's identity provider (OpenID Connect). Users of the tenant can
-- then sign in there; they are created on first sign-in and get roles from
-- their groups. The client secret is sealed with the master key.
CREATE TABLE identity_providers (
    tenant_id       uuid PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    -- Shown on the sign-in button.
    name            text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    issuer          text NOT NULL,
    client_id       text NOT NULL,
    client_secret   text NOT NULL,
    scopes          text NOT NULL DEFAULT 'openid profile email',
    username_claim  text NOT NULL DEFAULT 'preferred_username',
    groups_claim    text NOT NULL DEFAULT 'groups',
    -- Role for users none of whose groups is mapped; NULL denies them.
    default_role    text CHECK (default_role IN ('admin', 'operator', 'viewer')),
    -- [{"group": "...", "role": "admin|operator|viewer"}]
    role_mappings   jsonb NOT NULL DEFAULT '[]',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- Sign-ins in progress: what the callback needs to finish them. Single use,
-- short-lived.
CREATE TABLE oidc_logins (
    state_hash     text PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    nonce          text NOT NULL,
    pkce_verifier  text NOT NULL,
    return_to      text NOT NULL DEFAULT '/',
    expires_at     timestamptz NOT NULL
);

-- Limits on what a tenant's whole subtree may allocate. A missing row means
-- "no limit of its own" (ancestors' limits still apply).
CREATE TABLE quotas (
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    resource    text NOT NULL
                CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib',
                                    'load_balancers', 'vpcs', 'snapshot_gib', 'image_gib')),
    max         bigint NOT NULL CHECK (max >= 0),
    PRIMARY KEY (tenant_id, resource)
);

-- Provider resources a tenant may use. The root tenant may use everything; a
-- tenant can only pass on to its children what it was granted itself.
CREATE TABLE tenant_grants (
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    kind        text NOT NULL
                CHECK (kind IN ('cluster', 'storage_pool', 'external_network', 'backup_repository')),
    ref_id      uuid NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, kind, ref_id)
);

-- Who did what. No foreign keys: entries must outlive the tenants, users and
-- resources they mention.
CREATE TABLE audit_log (
    id           bigserial PRIMARY KEY,
    at           timestamptz NOT NULL DEFAULT now(),
    user_id      uuid,
    username     text,
    tenant_id    uuid,
    -- Materialised path of tenant_id at the time, for subtree queries.
    tenant_path  ltree,
    action       text NOT NULL,
    target_kind  text,
    target_id    uuid,
    outcome      text NOT NULL DEFAULT 'success' CHECK (outcome IN ('success', 'failure')),
    details      jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_by_path ON audit_log USING gist (tenant_path);
CREATE INDEX audit_log_by_time ON audit_log (at DESC);


-- ===================================================================
-- Tasks
-- ===================================================================

-- Durable long-running operations. Workers claim rows with
-- FOR UPDATE SKIP LOCKED and hold a lease; each step is idempotent so a task
-- whose lease expires can be resumed by another controller replica.
CREATE TABLE tasks (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    kind              text NOT NULL,
    target_kind       text,
    target_id         uuid,
    state             text NOT NULL DEFAULT 'pending'
                      CHECK (state IN ('pending', 'running', 'succeeded', 'failed', 'cancelled')),
    params            jsonb NOT NULL DEFAULT '{}',
    step              text,
    step_state        jsonb NOT NULL DEFAULT '{}',
    progress          real CHECK (progress BETWEEN 0 AND 1),
    error             text,
    created_by        uuid REFERENCES users (id) ON DELETE SET NULL,
    lease_owner       text,
    lease_expires_at  timestamptz,
    attempts          integer NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    started_at        timestamptz,
    finished_at       timestamptz
);
CREATE INDEX tasks_active ON tasks (created_at) WHERE state IN ('pending', 'running');
CREATE INDEX tasks_by_tenant ON tasks (tenant_id, created_at DESC);
CREATE INDEX tasks_by_target ON tasks (target_id, created_at DESC) WHERE target_id IS NOT NULL;


-- ===================================================================
-- Controllers, clusters and hosts
-- ===================================================================

-- Key material owned by the control plane (its CAs), shared through the
-- database so every controller replica uses the same. Private keys are
-- sealed with the master key.
CREATE TABLE pki (
    name        text PRIMARY KEY,
    cert_pem    text NOT NULL,
    key_pem     text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Controller replicas. Each registers the address its peers reach it on
-- (its agent endpoint, which also serves replica-to-replica calls) and
-- keeps a heartbeat. hosts.connected_to names the replica holding a host's
-- agent session.
CREATE TABLE replicas (
    id            text PRIMARY KEY,
    peer_address  text NOT NULL,
    started_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);

-- A cluster is a group of hosts that VMs are placed (and migrate) within.
-- Overcommit ratios scale hosts' physical CPUs and memory into what the
-- scheduler may allocate.
CREATE TABLE clusters (
    id                 uuid PRIMARY KEY,
    name               text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    cpu_overcommit     real NOT NULL DEFAULT 4.0 CHECK (cpu_overcommit > 0),
    memory_overcommit  real NOT NULL DEFAULT 1.0 CHECK (memory_overcommit > 0),
    created_at         timestamptz NOT NULL DEFAULT now(),
    -- Restart a failed host's VMs (those on shared storage) on the
    -- cluster's other hosts.
    ha_enabled         boolean NOT NULL DEFAULT false,
    -- Limits for each VM in the cluster, set by the provider so VMs sharing
    -- hosts and storage cannot starve each other. NULL: none.
    vm_net_mbps        integer CHECK (vm_net_mbps > 0),   -- each way, per NIC
    vm_disk_iops       integer CHECK (vm_disk_iops > 0),  -- per disk
    vm_disk_mbps       integer CHECK (vm_disk_mbps > 0)   -- per disk, MB/s
);
INSERT INTO clusters (id, name) VALUES (gen_random_uuid(), 'default');

CREATE TABLE hosts (
    id                        uuid PRIMARY KEY,
    name                      text NOT NULL,
    address                   inet NOT NULL,
    -- fenced: powered off after it failed; its VMs were moved. An operator
    -- puts it back in service once repaired. A decommissioned host keeps
    -- its record (and name) for the audit trail; the same machine may
    -- enroll again under its name.
    state                     text NOT NULL DEFAULT 'ready'
                              CHECK (state IN ('ready', 'maintenance', 'decommissioned', 'fenced')),
    -- Hosts outside any cluster are not scheduled on.
    cluster_id                uuid REFERENCES clusters (id) ON DELETE RESTRICT,
    cpu_count                 integer NOT NULL,
    memory_mib                bigint NOT NULL,
    cpu_model                 text NOT NULL DEFAULT '',
    -- Its host-model CPU (for cluster baselines), and as a feature list (to
    -- tell which VMs' CPU models it runs, with the CPU description it was
    -- worked out from).
    cpu_json                  text NOT NULL DEFAULT '',
    cpu_vendor                text,
    cpu_features              text[],
    cpu_features_of           text,
    kvm                       boolean NOT NULL DEFAULT false,
    libvirt_version           text NOT NULL DEFAULT '',
    ovn_chassis               text NOT NULL DEFAULT '',
    -- Physical networks its OVS bridges reach (from ovn-bridge-mappings);
    -- gateways for an external network are placed on hosts that have it.
    physical_networks         text[] NOT NULL DEFAULT '{}',
    -- QEMU's TLS is set up: it takes part in live migrations.
    migration_ready           boolean NOT NULL DEFAULT false,
    -- What it reports about itself that would give it more than it was
    -- accepted with (a new name or address, another OVN chassis, physical
    -- networks it did not have, more CPUs or memory) waits here until a
    -- provider administrator accepts it: a host taken over must not make
    -- itself a gateway for other tenants' VPCs, or attract their VMs.
    pending_facts             jsonb,
    agent_version             text NOT NULL DEFAULT '',
    -- The protocol revision its agent speaks (from its last session), and
    -- the commands it handles, for every replica to see.
    agent_protocol            integer NOT NULL DEFAULT 1,
    agent_capabilities        text[] NOT NULL DEFAULT '{}',
    -- Its certificate. After a renewal the previous one stays accepted, so
    -- an agent that has not yet stored the new one (it crashed in between)
    -- can still connect and renew again. Serial numbers, to revoke them.
    cert_fingerprint          text NOT NULL,
    cert_serial               text,
    cert_expires_at           timestamptz NOT NULL,
    previous_cert_fingerprint text,
    previous_cert_serial      text,
    previous_cert_expires_at  timestamptz,
    -- How to power it off when it stops responding, so its VMs can safely
    -- start elsewhere: `redfish` (a BMC) or `libvirt` (hosts that are VMs,
    -- as in the lab). Secrets in the config are sealed with the master key.
    fence_kind                text CHECK (fence_kind IN ('redfish', 'libvirt')),
    fence_config              jsonb NOT NULL DEFAULT '{}',
    -- Which controller replica holds the agent's session, if any, since
    -- when (it reconnects after renewing its certificate, a controller
    -- restart, a network break), and when it was last heard from.
    connected_to              text,
    connected_since           timestamptz,
    last_seen_at              timestamptz,
    created_at                timestamptz NOT NULL DEFAULT now(),
    updated_at                timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX hosts_name ON hosts (name) WHERE state <> 'decommissioned';
CREATE INDEX hosts_by_cluster ON hosts (cluster_id);
-- A chassis name belongs to one host: the controller signs OVN certificates
-- for the chassis a host reports, and OVN's RBAC trusts that name.
CREATE UNIQUE INDEX hosts_ovn_chassis ON hosts (ovn_chassis)
    WHERE ovn_chassis <> '' AND state <> 'decommissioned';

-- Single-use tokens hosts enroll with, joining the token's cluster.
CREATE TABLE join_tokens (
    id           text PRIMARY KEY,
    secret_hash  text NOT NULL,
    created_by   uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz,
    host_id      uuid REFERENCES hosts (id) ON DELETE SET NULL,
    cluster_id   uuid REFERENCES clusters (id) ON DELETE CASCADE
);

-- Revoked certificates of the controller's CA, for the revocation list
-- hosts install. An entry leaves the list once its certificate has expired.
CREATE TABLE revoked_certificates (
    serial      text PRIMARY KEY,
    host_id     uuid REFERENCES hosts (id) ON DELETE SET NULL,
    reason      text NOT NULL,
    revoked_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL
);


-- ===================================================================
-- Storage
-- ===================================================================

-- Where volumes (and images) live. `local` is each host's own disk and is
-- built in; shared pools (NFS, Ceph RBD) are reachable from the clusters
-- they are attached to. `config` holds the pool's connection details;
-- secrets in it (an RBD key) are sealed, and never returned by the API.
CREATE TABLE storage_pools (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    kind        text NOT NULL CHECK (kind IN ('local', 'nfs', 'rbd')),
    config      jsonb NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now()
);
-- The built-in local pool has a fixed ID (mst_core::ids::LOCAL_POOL).
INSERT INTO storage_pools (id, name, kind) VALUES ('00000000-0000-0000-0000-000000000001', 'local', 'local');

CREATE TABLE storage_pool_clusters (
    pool_id     uuid NOT NULL REFERENCES storage_pools (id) ON DELETE CASCADE,
    cluster_id  uuid NOT NULL REFERENCES clusters (id) ON DELETE CASCADE,
    PRIMARY KEY (pool_id, cluster_id)
);

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

-- Where backups are kept: NFS exports the hosts mount, set up by the
-- provider and granted to tenants like storage pools.
CREATE TABLE backup_repositories (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    kind        text NOT NULL CHECK (kind IN ('nfs')),
    config      jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);


-- ===================================================================
-- Images, VMs and their disks
-- ===================================================================

-- Images, downloaded once, as they are added, into the image store of the
-- pool they are kept in: the local disk of a host picked at random, or a
-- shared pool. VMs' disks are copies of them.
CREATE TABLE images (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    name              text NOT NULL,
    source_url        text NOT NULL,
    format            text NOT NULL CHECK (format IN ('qcow2', 'raw')),
    -- Visible to the owning tenant's whole subtree, not just the owner.
    shared            boolean NOT NULL DEFAULT false,
    storage_pool_id   uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000001'
                      REFERENCES storage_pools (id),
    -- 'downloading': being downloaded (a task); 'failed': that failed
    -- (`error` says why), and it can be downloaded again.
    state             text NOT NULL DEFAULT 'ready' CHECK (state IN ('downloading', 'ready', 'failed')),
    error             text,
    -- The host downloading it: a download resumed (a controller restarted)
    -- goes on there rather than starting over on another.
    download_host_id  uuid REFERENCES hosts (id) ON DELETE SET NULL,
    -- The stored file's size, and the size of the disk it holds.
    size_bytes        bigint,
    virtual_bytes     bigint,
    -- SHA-256 of the file downloaded from `source_url`, and of the disk it
    -- holds, read as a raw disk: every copy, and a download again, must
    -- hold the same.
    sha256            text,
    digest            text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX images_by_pool ON images (storage_pool_id);

-- Copies of an image: in a host's local image store (`host_id`, in the
-- local pool), or in a shared pool's (no host).
CREATE TABLE image_copies (
    image_id    uuid NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    host_id     uuid REFERENCES hosts (id) ON DELETE CASCADE,
    pool_id     uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000001'
                REFERENCES storage_pools (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT image_copies_where
        CHECK ((pool_id = '00000000-0000-0000-0000-000000000001') = (host_id IS NOT NULL))
);
CREATE UNIQUE INDEX image_copies_unique ON image_copies
    (image_id, pool_id, coalesce(host_id, '00000000-0000-0000-0000-000000000000'));
CREATE INDEX image_copies_by_host ON image_copies (host_id);
CREATE INDEX image_copies_by_pool ON image_copies (pool_id);

CREATE TABLE vms (
    id                    uuid PRIMARY KEY,
    tenant_id             uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    name                  text NOT NULL CHECK (name ~ '^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$'),
    host_id               uuid REFERENCES hosts (id) ON DELETE RESTRICT,
    spec                  jsonb NOT NULL,
    generation            bigint NOT NULL DEFAULT 1,
    -- Its CPU model's features (see hosts.cpu_features).
    cpu_features          text[],
    -- A migrating VM runs on both hosts for a while; its status says so,
    -- and what either host reports about it is taken with that in mind.
    status                text NOT NULL DEFAULT 'creating'
                          CHECK (status IN ('creating', 'ready', 'error', 'deleting', 'migrating')),
    desired_power         text NOT NULL DEFAULT 'running' CHECK (desired_power IN ('running', 'stopped')),
    -- What its host last reported about its power: the state, why a stopped
    -- or crashed VM stopped when libvirt knew (see DomainState.reason in
    -- proto/agent.proto), and when that last changed. Observations only;
    -- desired_power changes solely through the API.
    power                 text NOT NULL DEFAULT 'unknown',
    power_reason          text,
    power_observed_at     timestamptz,
    error                 text,
    -- Templates: stopped VMs kept to be cloned, never started themselves.
    template              boolean NOT NULL DEFAULT false,
    -- Its backup schedule: how often, how many scheduled backups to keep,
    -- and where; a full backup every so many (1: every one), counted with
    -- its incremental backups since its last full one.
    backup_schedule       text CHECK (backup_schedule IN ('daily', 'weekly')),
    backup_keep           integer CHECK (backup_keep BETWEEN 1 AND 365),
    backup_repository_id  uuid REFERENCES backup_repositories (id),
    backup_full_every     integer CHECK (backup_full_every BETWEEN 1 AND 365),
    backup_incrementals   integer NOT NULL DEFAULT 0,
    created_by            uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX vms_by_host ON vms (host_id);

-- VMs' disks, and disks of their own (named), attached to a VM or not.
CREATE TABLE volumes (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    vm_id       uuid REFERENCES vms (id) ON DELETE SET NULL,
    name        text CHECK (name IS NULL OR length(name) BETWEEN 1 AND 64),
    pool_id     uuid NOT NULL REFERENCES storage_pools (id) ON DELETE RESTRICT,
    -- Set for host-local pools; the volume only exists on that host.
    host_id     uuid REFERENCES hosts (id) ON DELETE RESTRICT,
    size_bytes  bigint NOT NULL CHECK (size_bytes > 0),
    -- While it is made, attached, detached, moved, grown or deleted on its
    -- own (not with a VM).
    state       text NOT NULL DEFAULT 'ready'
                CHECK (state IN ('creating', 'ready', 'attaching', 'detaching', 'moving', 'growing', 'deleting', 'failed')),
    error       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX volumes_by_vm ON volumes (vm_id);
CREATE INDEX volumes_by_tenant ON volumes (tenant_id);
CREATE UNIQUE INDEX volumes_name ON volumes (tenant_id, lower(name)) WHERE name IS NOT NULL;

-- Single-use, short-lived console tickets. Stored in the database so the
-- WebSocket may land on any controller replica.
CREATE TABLE console_tickets (
    token_hash  text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    vm_id       uuid NOT NULL REFERENCES vms (id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('vnc', 'serial')),
    expires_at  timestamptz NOT NULL
);

-- Migrations in progress, one per VM. The row reserves the VM's CPU and
-- memory on the destination until the VM moves there or the migration is
-- rolled back.
CREATE TABLE migrations (
    vm_id             uuid PRIMARY KEY REFERENCES vms (id) ON DELETE CASCADE,
    task_id           uuid NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    source_host_id    uuid NOT NULL REFERENCES hosts (id),
    dest_host_id      uuid NOT NULL REFERENCES hosts (id),
    -- preparing: setting up the destination; running: libvirt is migrating;
    -- committing: the VM runs on the destination, cleanup remains.
    state             text NOT NULL DEFAULT 'preparing'
                      CHECK (state IN ('preparing', 'running', 'committing')),
    vcpus             integer NOT NULL,
    memory_mib        bigint NOT NULL,
    cancel_requested  boolean NOT NULL DEFAULT false,
    -- How far the controller driving it step by step has got (the step
    -- reached, and what it handed on), so a replica that takes the task
    -- over carries on, or undoes it, from there.
    handoff           jsonb NOT NULL DEFAULT '{}',
    started_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_host_id <> dest_host_id)
);
CREATE INDEX migrations_by_dest ON migrations (dest_host_id);

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
    -- What it counts against quotas: it keeps every block overwritten
    -- since, so it is charged the VM's disks' size when taken (what it
    -- grows to cannot be known in advance).
    size_bytes  bigint NOT NULL DEFAULT 0,
    -- Whether its file systems were consistent: the VM was stopped, or its
    -- guest agent froze them. Otherwise as after a crash.
    consistent  boolean,
    error       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (vm_id, name)
);
CREATE INDEX snapshots_tenant ON snapshots (tenant_id);

-- Backups: copies of VMs' disks in backup repositories, apart from the
-- storage the VMs run on. They outlive their VM. One may hold only what
-- changed since an earlier one of the same VM (its parent), each disk a
-- qcow2 file backed by the parent's copy; deleting a backup merges it into
-- the one built on it.
CREATE TABLE backups (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id),
    -- The VM it was taken of, while it exists; its name and spec stay.
    vm_id          uuid REFERENCES vms (id) ON DELETE SET NULL,
    vm_name        text NOT NULL,
    repository_id  uuid NOT NULL REFERENCES backup_repositories (id),
    -- 'deleting': being merged into the backup built on it, and removed;
    -- its files may no longer be as taken.
    state          text NOT NULL CHECK (state IN ('running', 'ready', 'failed', 'deleting')),
    -- How it was taken: in full, or as the changes since its parent; and
    -- why one asked to be incremental was taken in full.
    kind           text NOT NULL DEFAULT 'full' CHECK (kind IN ('full', 'incremental')),
    full_reason    text,
    -- The backup it builds on now (merges move this down the chain); NULL:
    -- it stands alone.
    parent_id      uuid REFERENCES backups (id),
    -- The backups built on one another, named after the first; work on a
    -- chain (taking, deleting) is serialized on it.
    chain_id       uuid NOT NULL,
    -- Taken by the VM's schedule (and pruned by it), or by hand.
    scheduled      boolean NOT NULL DEFAULT false,
    -- The VM's spec when it was taken, for restoring it.
    spec           jsonb NOT NULL,
    -- The disks' virtual sizes added up: what it counts against quotas.
    size_bytes     bigint NOT NULL,
    -- What it takes up in the repository, once taken.
    stored_bytes   bigint,
    -- Whether its file systems were consistent: the VM was stopped, or its
    -- guest agent froze them for the start. Otherwise as after a crash.
    consistent     boolean,
    error          text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    completed_at   timestamptz
);
CREATE INDEX backups_vm ON backups (vm_id, created_at);
CREATE INDEX backups_tenant ON backups (tenant_id, created_at);
CREATE INDEX backups_parent ON backups (parent_id);


-- ===================================================================
-- Networks
-- ===================================================================

-- Provider networks outside OVN (a VLAN or flat segment on each host's
-- bridge), reached through VPC routers.
CREATE TABLE external_networks (
    id                uuid PRIMARY KEY,
    name              text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 64),
    -- Name in the hosts' ovn-bridge-mappings.
    physical_network  text NOT NULL,
    cidr              cidr NOT NULL,
    gateway           inet NOT NULL,
    -- Addresses metastratum hands out: VPC gateways, floating IPs and load
    -- balancers' addresses.
    pool_start        inet NOT NULL,
    pool_end          inet NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (family(cidr) = 4 AND gateway << cidr AND pool_start << cidr AND pool_end << cidr
           AND pool_start <= pool_end)
);

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

CREATE TABLE networks (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    vpc_id      uuid NOT NULL REFERENCES vpcs (id) ON DELETE RESTRICT,
    name        text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    cidr        cidr NOT NULL,
    gateway     inet NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name),
    CHECK (family(cidr) = 4 AND gateway << cidr),
    -- Routed together, so their ranges must not overlap.
    CONSTRAINT networks_no_overlap EXCLUDE USING gist (vpc_id WITH =, cidr inet_ops WITH &&)
);

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

-- A VM's network interfaces. A port's IP belongs to its network; a network
-- is not deleted while ports remain.
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

-- Addresses on external networks: a VPC's SNAT address, a floating IP, or
-- a load balancer's.
CREATE TABLE external_ips (
    id                   uuid PRIMARY KEY,
    external_network_id  uuid NOT NULL REFERENCES external_networks (id) ON DELETE RESTRICT,
    ip                   inet NOT NULL,
    kind                 text NOT NULL CHECK (kind IN ('gateway', 'floating', 'load_balancer')),
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

-- Guardrails: deny rules for every VM in a tenant's subtree, which security
-- groups below cannot override. They belong to the tenant whose
-- administrators set them (`owner_tenant_id`), which may be above the
-- tenant they apply to: only they remove them.
CREATE TABLE network_policies (
    id               uuid PRIMARY KEY,
    tenant_id        uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    owner_tenant_id  uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    direction        text NOT NULL CHECK (direction IN ('ingress', 'egress')),
    protocol         text NOT NULL CHECK (protocol IN ('any', 'tcp', 'udp', 'icmp')),
    port_min         integer CHECK (port_min BETWEEN 1 AND 65535),
    port_max         integer CHECK (port_max BETWEEN 1 AND 65535),
    remote_cidr      cidr NOT NULL CHECK (family(remote_cidr) = 4),
    description      text NOT NULL DEFAULT '',
    created_by       uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    CHECK ((port_min IS NULL) = (port_max IS NULL) AND port_min <= port_max),
    CHECK (protocol IN ('tcp', 'udp') OR port_min IS NULL)
);
CREATE INDEX network_policies_by_tenant ON network_policies (tenant_id);

-- Load balancers: a virtual IP whose listeners spread connections over
-- member VMs, by OVN in the VPC's router and switches.
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


-- ===================================================================
-- Live updates
-- ===================================================================

-- Every change to a watched table is announced on the `mst_events` channel,
-- so each controller replica can tell its browsers what to refresh.
-- Notifications carry what the row is and which tenant owns it (for
-- authorization), never its contents. Some rows are announced as changes
-- to another: a migration to its VM, a tenant's domain to the tenant, a
-- security group rule to its group.
CREATE FUNCTION mst_notify_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rec record;
    row_id uuid;
    tenant_path text;
    shared boolean;
BEGIN
    IF TG_OP = 'DELETE' THEN
        rec := OLD;
    ELSE
        rec := NEW;
    END IF;
    IF TG_TABLE_NAME = 'migrations' THEN
        row_id := rec.vm_id;
    ELSIF TG_TABLE_NAME = 'tenant_domains' THEN
        row_id := rec.tenant_id;
    ELSE
        row_id := rec.id;
    END IF;
    IF TG_TABLE_NAME = 'tenants' THEN
        tenant_path := rec.path::text;
    ELSIF TG_TABLE_NAME IN ('hosts', 'clusters', 'storage_pools', 'external_networks', 'backup_repositories') THEN
        -- Provider infrastructure: no owning tenant.
        tenant_path := NULL;
    ELSIF TG_TABLE_NAME = 'security_group_rules' THEN
        row_id := rec.security_group_id;
        SELECT t.path::text INTO tenant_path
        FROM security_groups g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = row_id;
    ELSIF TG_TABLE_NAME = 'migrations' THEN
        SELECT t.path::text INTO tenant_path FROM vms v JOIN tenants t ON t.id = v.tenant_id WHERE v.id = row_id;
    ELSE
        SELECT path::text INTO tenant_path FROM tenants WHERE tenants.id = rec.tenant_id;
    END IF;
    -- Images are seen below their tenant only while shared: say whether
    -- this one is (or was, as it stops being).
    IF TG_TABLE_NAME = 'images' THEN
        shared := rec.shared OR (TG_OP = 'UPDATE' AND OLD.shared);
    END IF;
    PERFORM pg_notify('mst_events', json_build_object(
        'kind', TG_ARGV[0],
        'id', row_id,
        'op', CASE WHEN TG_TABLE_NAME IN ('security_group_rules', 'migrations', 'tenant_domains') THEN 'update' ELSE lower(TG_OP) END,
        'tenant', tenant_path,
        'shared', shared
    )::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER tenants_notify AFTER INSERT OR UPDATE OR DELETE ON tenants
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('tenant');
CREATE TRIGGER tenant_domains_notify AFTER INSERT OR UPDATE OR DELETE ON tenant_domains
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('tenant');
CREATE TRIGGER clusters_notify AFTER INSERT OR UPDATE OR DELETE ON clusters
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('cluster');
CREATE TRIGGER storage_pools_notify AFTER INSERT OR UPDATE OR DELETE ON storage_pools
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('storage_pool');
CREATE TRIGGER backup_repositories_notify AFTER INSERT OR UPDATE OR DELETE ON backup_repositories
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('backup_repository');
CREATE TRIGGER images_notify AFTER INSERT OR UPDATE OR DELETE ON images
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('image');
CREATE TRIGGER vms_notify AFTER INSERT OR UPDATE OR DELETE ON vms
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vm');
CREATE TRIGGER migrations_notify AFTER INSERT OR UPDATE OR DELETE ON migrations
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vm');
CREATE TRIGGER volumes_notify AFTER INSERT OR UPDATE OR DELETE ON volumes
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('disk');
CREATE TRIGGER snapshots_notify AFTER INSERT OR UPDATE OR DELETE ON snapshots
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('snapshot');
CREATE TRIGGER backups_notify AFTER INSERT OR UPDATE OR DELETE ON backups
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('backup');
CREATE TRIGGER external_networks_notify AFTER INSERT OR UPDATE OR DELETE ON external_networks
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('external_network');
CREATE TRIGGER vpcs_notify AFTER INSERT OR UPDATE OR DELETE ON vpcs
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('vpc');
CREATE TRIGGER networks_notify AFTER INSERT OR UPDATE OR DELETE ON networks
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('network');
CREATE TRIGGER external_ips_notify AFTER INSERT OR UPDATE OR DELETE ON external_ips
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('external_ip');
CREATE TRIGGER security_groups_notify AFTER INSERT OR UPDATE OR DELETE ON security_groups
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('security_group');
CREATE TRIGGER security_group_rules_notify AFTER INSERT OR UPDATE OR DELETE ON security_group_rules
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('security_group');
CREATE TRIGGER network_policies_notify AFTER INSERT OR UPDATE OR DELETE ON network_policies
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('network_policy');
CREATE TRIGGER load_balancers_notify AFTER INSERT OR UPDATE OR DELETE ON load_balancers
    FOR EACH ROW EXECUTE FUNCTION mst_notify_change('load_balancer');

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
