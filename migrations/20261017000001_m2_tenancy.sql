-- M2: role bindings, API tokens, quotas, resource grants and the audit log.

-- A user holds a role at a tenant; it applies to that tenant and every tenant
-- below it. Roles are built in (see mst-authz).
CREATE TABLE role_bindings (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    role        text NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, tenant_id, role)
);
CREATE INDEX role_bindings_by_tenant ON role_bindings (tenant_id);

-- M1 let every user act on their home tenant's subtree; keep exactly that.
INSERT INTO role_bindings (id, user_id, tenant_id, role)
SELECT gen_random_uuid(), id, tenant_id, 'admin' FROM users;

-- Long-lived credentials for automation. They act as their user, with that
-- user's role bindings. Only a hash of the secret is stored.
CREATE TABLE api_tokens (
    id            uuid PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name          text NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    token_hash    text NOT NULL UNIQUE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz,
    last_used_at  timestamptz
);
CREATE INDEX api_tokens_by_user ON api_tokens (user_id);

-- Limits on what a tenant's whole subtree may allocate. A missing row, or a
-- NULL limit, means "no limit of its own" (ancestors' limits still apply).
CREATE TABLE quotas (
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    resource    text NOT NULL CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib')),
    max         bigint NOT NULL CHECK (max >= 0),
    PRIMARY KEY (tenant_id, resource)
);

-- Provider resources a tenant may use. The root tenant may use everything; a
-- tenant can only pass on to its children what it was granted itself.
CREATE TABLE tenant_grants (
    tenant_id   uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('cluster', 'storage_pool')),
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
