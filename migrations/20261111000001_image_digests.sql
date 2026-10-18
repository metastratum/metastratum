-- VMs' disks always stand on their own (0.5 refuses older agents, which
-- put them on cached images), and migrations always copy local disks
-- whole: what recorded otherwise goes.
ALTER TABLE volumes DROP COLUMN image_id;
ALTER TABLE migrations DROP COLUMN full_copy;

-- The SHA-256 of the disk an image holds, read as a raw disk: every copy,
-- and a download again, must hold the same.
ALTER TABLE images ADD COLUMN digest text;
-- The host downloading it: a download resumed (a controller restarted)
-- goes on there rather than starting over on another.
ALTER TABLE images ADD COLUMN download_host_id uuid REFERENCES hosts (id) ON DELETE SET NULL;
