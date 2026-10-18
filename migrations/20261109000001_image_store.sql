-- The image store: an image is downloaded once, when it is added, onto a
-- host, and copied from host to host from there. VMs' disks are copies of
-- it that depend on nothing. Volumes made before have the image as their
-- backing file (or clone parent, on Ceph): `volumes.image_id` now names only
-- such an image, which must then stay.

-- 'downloading': being downloaded (a task); 'failed': that failed (`error`
-- says why), and it can be downloaded again. Images added before are ready:
-- the first host to make a disk of one downloads it.
ALTER TABLE images ADD COLUMN state text NOT NULL DEFAULT 'ready'
    CHECK (state IN ('downloading', 'ready', 'failed'));
ALTER TABLE images ADD COLUMN error text;
-- The stored file's size, and the size of the disk it holds.
ALTER TABLE images ADD COLUMN size_bytes bigint;
ALTER TABLE images ADD COLUMN virtual_bytes bigint;
-- SHA-256 of the file downloaded from `source_url`: downloaded again (no
-- host with a copy left), it must be the same file.
ALTER TABLE images ADD COLUMN sha256 text;

-- The hosts whose image store holds a copy.
CREATE TABLE image_copies (
    image_id    uuid NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    host_id     uuid NOT NULL REFERENCES hosts (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (image_id, host_id)
);
CREATE INDEX image_copies_by_host ON image_copies (host_id);

-- Images stay on hosts as long as they exist: tenants' are charged their
-- stored size.
ALTER TABLE quotas DROP CONSTRAINT quotas_resource_check;
ALTER TABLE quotas ADD CONSTRAINT quotas_resource_check
    CHECK (resource IN ('vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib',
                        'load_balancers', 'vpcs', 'snapshot_gib', 'image_gib'));

-- A migration that copies local disks whole, making them standalone on the
-- destination.
ALTER TABLE migrations ADD COLUMN full_copy boolean NOT NULL DEFAULT false;
