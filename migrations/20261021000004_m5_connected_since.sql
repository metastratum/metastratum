-- When the host's current agent session began (it reconnects after
-- renewing its certificate, a controller restart, a network break).
ALTER TABLE hosts ADD COLUMN connected_since timestamptz;
