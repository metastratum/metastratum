-- A decommissioned host keeps its record (and name) for the audit trail;
-- the same machine may enroll again under its name.
ALTER TABLE hosts DROP CONSTRAINT hosts_name_key;
CREATE UNIQUE INDEX hosts_name ON hosts (name) WHERE state <> 'decommissioned';
