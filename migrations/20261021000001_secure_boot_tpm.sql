-- Secure Boot and TPMs for guests: what hosts can give them, what images'
-- VMs get by default, and the account a Windows image's VMs are signed in
-- with by default (none: who makes the VM names one).
ALTER TABLE hosts
    ADD COLUMN tpm boolean NOT NULL DEFAULT false,
    ADD COLUMN secure_boot boolean NOT NULL DEFAULT false;
ALTER TABLE images
    ADD COLUMN secure_boot boolean NOT NULL DEFAULT false,
    ADD COLUMN tpm boolean NOT NULL DEFAULT false,
    ADD COLUMN admin_user text CHECK (admin_user <> '');

-- The latest copy of each VM's TPM state, sealed with the master key: what
-- a VM restarted on another host after its own failed gets. (Between hosts
-- that are up, it is copied host to host.)
CREATE TABLE vm_tpm_states (
    vm_id       uuid PRIMARY KEY REFERENCES vms (id) ON DELETE CASCADE,
    state       text NOT NULL,
    digest      text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
