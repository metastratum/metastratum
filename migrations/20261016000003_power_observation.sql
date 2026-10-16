-- What a host last reported about a VM's power, with context. These columns
-- are observations only; desired_power changes solely through the API.

ALTER TABLE vms
    -- Why a stopped or crashed VM stopped, when libvirt knew (see
    -- DomainState.reason in proto/agent.proto).
    ADD COLUMN power_reason text,
    -- When the reported power state (or its reason) last changed.
    ADD COLUMN power_observed_at timestamptz;
