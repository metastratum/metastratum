-- Images can be uploaded instead of downloaded from a URL. An upload is
-- `uploading` while its parts arrive, on the host it is uploaded to
-- (`download_host_id`): `upload_bytes` is its size, `uploaded_bytes` how
-- much is there, `upload_touched_at` when the last part came (one left
-- untouched fails). Once all of it is there it is added to the store as a
-- download is (`downloading`). An uploaded image has no `source_url`.
ALTER TABLE images DROP CONSTRAINT images_state_check;
ALTER TABLE images ADD CONSTRAINT images_state_check
    CHECK (state IN ('uploading', 'downloading', 'ready', 'failed'));
ALTER TABLE images
    ADD COLUMN upload_bytes bigint CHECK (upload_bytes > 0),
    ADD COLUMN uploaded_bytes bigint NOT NULL DEFAULT 0
        CHECK (uploaded_bytes >= 0 AND uploaded_bytes <= coalesce(upload_bytes, 0)),
    ADD COLUMN upload_touched_at timestamptz,
    ADD CONSTRAINT images_source CHECK ((upload_bytes IS NULL) = (source_url <> ''));
