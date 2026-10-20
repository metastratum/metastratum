-- 0.5.0 downloaded an image again when no copy of it was left (every host
-- holding one decommissioned). Images are no longer downloaded again: one
-- without a copy anywhere has failed, for good. (Its URL is in the audit
-- log, `image.create`, for adding it again.)
UPDATE images i SET state = 'failed',
       error = 'no copy of it is left (the hosts holding one were decommissioned), and images are no longer downloaded again: delete it and add it again'
 WHERE i.state = 'ready'
   AND NOT EXISTS (SELECT 1 FROM image_copies c LEFT JOIN hosts h ON h.id = c.host_id
                   WHERE c.image_id = i.id AND (h.id IS NULL OR h.state <> 'decommissioned'));
