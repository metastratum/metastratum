-- Whether VMs made from an image may sleep (suspend to RAM or to disk) by
-- default, as on hardware: desktops do when idle. Each VM keeps its own
-- setting in its spec.
ALTER TABLE images ADD COLUMN sleep boolean NOT NULL DEFAULT true;
