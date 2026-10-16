-- Serial numbers of hosts' certificates, so they can be revoked.
ALTER TABLE hosts ADD COLUMN cert_serial text,
                  ADD COLUMN previous_cert_serial text,
                  ADD COLUMN previous_cert_expires_at timestamptz;

-- Revoked certificates of the controller's CA, for the revocation list
-- hosts install. An entry leaves the list once its certificate has expired.
CREATE TABLE revoked_certificates (
    serial      text PRIMARY KEY,
    host_id     uuid REFERENCES hosts (id) ON DELETE SET NULL,
    reason      text NOT NULL,
    revoked_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL
);
