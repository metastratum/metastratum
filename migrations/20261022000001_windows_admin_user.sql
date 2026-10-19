-- Windows images added before accounts could be named are Windows Server
-- images, whose VMs were signed in to as Administrator: they keep that.
UPDATE images SET admin_user = 'Administrator' WHERE os = 'windows' AND admin_user IS NULL;
