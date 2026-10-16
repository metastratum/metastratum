-- M2: per-tenant OpenID Connect sign-in.

-- A tenant's identity provider. Users of the tenant can then sign in there;
-- they are created on first sign-in and get roles from their groups.
-- TODO(M5): encrypt client secrets at rest.
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

-- Users from an identity provider are matched by its stable subject ID.
ALTER TABLE users ADD COLUMN external_subject text;
CREATE UNIQUE INDEX users_external_subject ON users (tenant_id, external_subject) WHERE external_subject IS NOT NULL;

-- Bindings the identity provider's groups produced are re-synced at every
-- sign-in; manual ones are left alone.
ALTER TABLE role_bindings ADD COLUMN source text NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'oidc'));
