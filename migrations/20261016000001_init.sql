-- Core schema: the tenant tree, users and durable tasks.

CREATE EXTENSION IF NOT EXISTS ltree;

-- Tenants form a tree rooted at the provider. `path` is the materialised
-- lineage (root first) whose labels are tenant IDs in simple hex form; it is
-- maintained by trigger so callers only ever supply `parent_id`.
CREATE TABLE tenants (
    id          uuid PRIMARY KEY,
    parent_id   uuid REFERENCES tenants (id) ON DELETE RESTRICT,
    slug        text NOT NULL CHECK (slug ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    name        text NOT NULL,
    path        ltree NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
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

-- Users belong to a home tenant. Local users have a password hash; users that
-- come from a tenant's OIDC provider do not.
CREATE TABLE users (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    username       text NOT NULL CHECK (username ~ '^[A-Za-z0-9._@-]{1,128}$'),
    display_name   text NOT NULL DEFAULT '',
    email          text,
    password_hash  text,
    disabled       boolean NOT NULL DEFAULT false,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, username)
);

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
