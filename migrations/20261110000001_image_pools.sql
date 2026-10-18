-- Images are kept in a storage pool of their tenant's choosing: on the
-- local disk of a host picked at random (as before), or once in a shared
-- pool, which the hosts attached to it make disks from directly.
ALTER TABLE images ADD COLUMN storage_pool_id uuid NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001' REFERENCES storage_pools (id);
CREATE INDEX images_by_pool ON images (storage_pool_id);

-- A copy is in a host's local image store (`host_id`, in the local pool),
-- or in a shared pool's (no host).
ALTER TABLE image_copies ADD COLUMN pool_id uuid NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001' REFERENCES storage_pools (id);
ALTER TABLE image_copies DROP CONSTRAINT image_copies_pkey;
ALTER TABLE image_copies ALTER COLUMN host_id DROP NOT NULL;
ALTER TABLE image_copies ADD CONSTRAINT image_copies_where
    CHECK ((pool_id = '00000000-0000-0000-0000-000000000001') = (host_id IS NOT NULL));
CREATE UNIQUE INDEX image_copies_unique ON image_copies
    (image_id, pool_id, coalesce(host_id, '00000000-0000-0000-0000-000000000000'));
CREATE INDEX image_copies_by_pool ON image_copies (pool_id);
