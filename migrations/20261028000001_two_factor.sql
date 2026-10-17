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

-- Password accounts of the tenant and everything below it must use a
-- second factor; those without one can only add one.
ALTER TABLE tenants ADD COLUMN require_two_factor boolean NOT NULL DEFAULT false;
