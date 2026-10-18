-- Incremental backups: a backup may hold only what changed since an earlier
-- one of the same VM (its parent), each disk a qcow2 file backed by the
-- parent's copy. Deleting a backup merges it into the one built on it.

-- How it was taken: in full, or as the changes since its parent.
ALTER TABLE backups ADD COLUMN kind text NOT NULL DEFAULT 'full'
    CHECK (kind IN ('full', 'incremental'));
-- The backup it builds on now (merges move this down the chain); NULL: it
-- stands alone.
ALTER TABLE backups ADD COLUMN parent_id uuid REFERENCES backups (id);
-- The backups built on one another, named after the first; work on a chain
-- (taking, deleting) is serialized on it.
ALTER TABLE backups ADD COLUMN chain_id uuid;
UPDATE backups SET chain_id = id;
ALTER TABLE backups ALTER COLUMN chain_id SET NOT NULL;
-- Why a backup asked to be incremental was taken in full.
ALTER TABLE backups ADD COLUMN full_reason text;
CREATE INDEX backups_parent ON backups (parent_id);

-- 'deleting': being merged into the backup built on it, and removed; its
-- files may no longer be as taken.
ALTER TABLE backups DROP CONSTRAINT backups_state_check;
ALTER TABLE backups ADD CONSTRAINT backups_state_check
    CHECK (state IN ('running', 'ready', 'failed', 'deleting'));

-- A schedule's full backup every so many backups (1: every one), counted
-- with the VM's incremental backups since its last full one. Schedules from
-- before incremental backups took full ones.
ALTER TABLE vms ADD COLUMN backup_full_every integer
    CHECK (backup_full_every BETWEEN 1 AND 365);
ALTER TABLE vms ADD COLUMN backup_incrementals integer NOT NULL DEFAULT 0;
UPDATE vms SET backup_full_every = 1 WHERE backup_schedule IS NOT NULL;
