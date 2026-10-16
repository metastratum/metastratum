-- How far a migration the controller drives step by step has got (the
-- step reached, and what it handed on), so a replica that takes the task
-- over carries on, or undoes it, from there.
ALTER TABLE migrations ADD COLUMN handoff jsonb NOT NULL DEFAULT '{}';

-- The protocol revision the host's agent speaks (from its last session):
-- whether migrations to and from it can be driven by the controller.
ALTER TABLE hosts ADD COLUMN agent_protocol integer NOT NULL DEFAULT 1;
