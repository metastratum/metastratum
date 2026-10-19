-- Images come to a host one way: received there as a file, in parts, from
-- whoever uploads it or from the controller downloading it from a URL as it
-- is added (the only time it is downloaded: no URL is kept anywhere). The
-- host then adds it to the store (task `image.store`).
--
-- States: `downloading` (the controller fetching it) or `uploading` (its
-- parts coming), `storing` (the host checking it and adding it to the
-- store), `ready`, `failed` (for good: deleted and added again).

-- What hosts were downloading themselves cannot go on (their URL goes).
UPDATE images SET state = 'failed',
       error = 'adding it was cut off by an upgrade; delete it and add it again'
 WHERE state = 'downloading' AND upload_bytes IS NULL;
UPDATE tasks SET state = 'failed', finished_at = now(), updated_at = now(),
       error = 'cut off by an upgrade: images are no longer downloaded by hosts'
 WHERE kind = 'image.download' AND state IN ('pending', 'running')
   AND NOT EXISTS (SELECT 1 FROM images i WHERE i.id = tasks.target_id AND i.upload_bytes IS NOT NULL);
UPDATE tasks SET kind = 'image.store' WHERE kind = 'image.download';

ALTER TABLE images DROP COLUMN source_url;
ALTER TABLE images ADD COLUMN origin text NOT NULL DEFAULT 'download'
    CHECK (origin IN ('download', 'upload'));
UPDATE images SET origin = 'upload' WHERE upload_bytes IS NOT NULL;
ALTER TABLE images ALTER COLUMN origin DROP DEFAULT;

-- The file's size (known once it is all there, if not before) and how much
-- of it has come; when a part last came.
ALTER TABLE images DROP CONSTRAINT images_check;
ALTER TABLE images RENAME COLUMN upload_bytes TO file_bytes;
ALTER TABLE images RENAME COLUMN uploaded_bytes TO received_bytes;
ALTER TABLE images RENAME COLUMN upload_touched_at TO received_at;
ALTER TABLE images ADD CONSTRAINT images_received
    CHECK (received_bytes >= 0 AND (file_bytes IS NULL OR received_bytes <= file_bytes));

ALTER TABLE images DROP CONSTRAINT images_state_check;
UPDATE images SET state = 'storing' WHERE state = 'downloading';
ALTER TABLE images ADD CONSTRAINT images_state_check
    CHECK (state IN ('downloading', 'uploading', 'storing', 'ready', 'failed'));
-- An upload has its size from the start.
ALTER TABLE images ADD CONSTRAINT images_upload_size
    CHECK (origin <> 'upload' OR file_bytes IS NOT NULL);
