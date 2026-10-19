-- The operating system an image holds: Windows guests get Hyper-V
-- enlightenments, and their password the way cloudbase-init takes it.
ALTER TABLE images ADD COLUMN os text NOT NULL DEFAULT 'linux'
    CHECK (os IN ('linux', 'windows'));
