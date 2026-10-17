-- Templates: stopped VMs kept to be cloned, never started themselves.
ALTER TABLE vms ADD COLUMN template boolean NOT NULL DEFAULT false;
