-- M5: several controller replicas. Each registers the address its peers
-- reach it on (its agent endpoint, which also serves replica-to-replica
-- calls) and keeps a heartbeat. hosts.connected_to names the replica holding
-- a host's agent session.
CREATE TABLE replicas (
    id            text PRIMARY KEY,
    peer_address  text NOT NULL,
    started_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now()
);
