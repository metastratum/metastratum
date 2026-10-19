# Controllers

Run as many replicas as you like (two or three is plenty), each with the
same database, master key and OVN servers. `deploy/systemd/mst-controller.service`
is a unit for them. Their settings are in `/etc/metastratum/controller.toml`,
with the master key beside it as `master.key`; the unit passes both in as
credentials, readable by the controller alone. A typical file:

```toml
database_url = "postgres://metastratum:...@db.example.net/metastratum?sslmode=verify-full"
metrics_listen = "10.0.0.21:7822"

[api]
trusted_proxies = ["10.0.0.5", "10.0.0.6"]
redis_url = "rediss://:...@redis.example.net:6380"

[ovn]
northbound = ["ssl:10.0.0.31:6641", "ssl:10.0.0.32:6641", "ssl:10.0.0.33:6641"]
southbound = ["ssl:10.0.0.31:6642", "ssl:10.0.0.32:6642", "ssl:10.0.0.33:6642"]
```

Everything else has a default: the API on `127.0.0.1:7820` (for the proxy
in front), the agent endpoint on port 7821 (out of the way of the ports
other services usually take, such as 8080 and 8443).
`deploy/config/controller.toml`
lists every setting. A setting the controller does not know is an error, so
a typo is caught when it starts. The other commands (`bootstrap`,
`join-token`, `ovn-certificate`) read the same file.

**The master key** seals the secrets kept in Postgres: the internal CA's
key, storage credentials, identity provider client secrets and BMC
passwords. Every replica needs the same key. Keep it out of the database's
backups, and back it up separately: a database restored without its key has
lost those secrets (the CA key among them, so every host would have to
enroll again).

**Web traffic.** The controller serves plain HTTP; put a TLS-terminating
proxy or load balancer in front of the replicas. No stickiness is needed.
The proxy must:

- pass WebSocket upgrades (`/api/v1/console/...`, the virtual consoles);
- not buffer `/api/v1/events` (Server-Sent Events, the UI's live updates),
  and allow idle connections of a few minutes on both;
- be listed in `api.trusted_proxies`, and set `X-Forwarded-For` (so rate
  limits see clients rather than the proxy) and `X-Forwarded-Proto`,
  replacing whatever the client sent. `X-Forwarded-Proto: https` marks the
  session cookie `Secure` and turns on Strict-Transport-Security;
- pass the browser's `Host` header on (HAProxy does; nginx needs
  `proxy_set_header Host $host`), or set `X-Forwarded-Host`. With
  `X-Forwarded-Proto` it makes the redirect URI for OIDC sign-in,
  `https://<host>/api/v1/auth/oidc/callback`, which is what to register
  with identity providers (Tenants → Sign-in shows it);
- take request bodies of 32 MiB: image uploads come in parts of 16 MiB (in
  nginx, `client_max_body_size 32m`; HAProxy and Caddy take them as they
  are);
- reach `/healthz`, which answers 200 while the replica can reach Postgres.

**Tenant domains.** A tenant can have its own address, such as
`mst.acme.com` for Acme (Tenants → Acme → Sign-in → Domains; provider
administrators only). The sign-in page there starts from Acme: Acme's people
leave the tenant empty, and those of its sub-tenants enter the path below
it (`dev` for `acme/dev`). Nothing above or beside Acme can be signed in
to at that address, and Acme and its sub-tenants can be signed in to
nowhere else: at the main address, or at a domain above Acme's, they don't
exist. A sub-tenant with a domain of its own leaves Acme's address the same
way. The page asks for a tenant only when there are sub-tenants to sign in
to there. Provider accounts always sign in at the main address, even when
the provider tenant has a domain too. For each domain:

- point its DNS at the proxy, and give the proxy a certificate for it;
- if the tenant signs in through an identity provider, register
  `https://mst.acme.com/api/v1/auth/oidc/callback` with it.

A tenant's Sign-in tab shows where its people sign in, and the redirect URI
to register: at the domain bound nearest above it (or to it), so for
`acme/dev` that is `https://mst.acme.com/login?tenant=dev`. Without a
domain, both are at the address you are using.

The controllers need no setting: they read the name from the request, as
for the redirect URI above.

**Agent traffic** does not go through that proxy. Each host dials a
replica's agent endpoint (`agents.listen`, gRPC with mutual TLS against the
controller's own CA) and keeps one session open. Give the agents every
replica in `/etc/metastratum/agent.toml`
(`controllers = ["https://ctl1.example.net:7821", "https://ctl2.example.net:7821"]`,
see `deploy/config/agent.toml`); an agent that loses its replica moves to
the next. Agents check the endpoint's certificate against the controller's
CA, not its name, so any name or address that reaches a replica will do.

A request or task may land on a replica other than the one holding the
host's session. It then forwards the command, or console stream, to that
replica over the agent endpoint, so replicas must reach each other, and
authenticate each other with certificates the CA issues them at startup.
Each finds its own address for that from the route to the database (set
`agents.peer_address` if the others reach it elsewhere).

**Work shared between replicas.** Tasks (creating, migrating VMs...) are
claimed from Postgres by any replica with a lease, and resumed by another
if their replica dies. Network reconciliation runs on whichever replica
gets a change first, serialized by a database lock. Watching hosts for
failure (below) runs on one replica at a time, the leader, chosen with a
Postgres advisory lock that is released when its session ends.

## Checking an installation

`mst-controller doctor` looks at a running installation against the
deployment checklist in [SECURITY.md](../../SECURITY.md#deployment-checklist):

- the configuration and master key readable by the controller alone;
- the API on a loopback or private address behind a trusted HTTPS proxy,
  Redis with several controllers, metrics on a private address;
- PostgreSQL over TLS with its certificate checked;
- both OVN databases over TLS, with certificates from metastratum's CAs that
  are not about to expire, and no plain listeners left on the OVN nodes;
- hosts connected with current agents, fencing on every host of clusters
  with high availability, per-VM limits on clusters, API tokens that expire.

Each finding says how to fix it. It exits 0 when everything checked is in
order, 1 with warnings and 2 with failures, so it can run from monitoring
too. Run it on a controller (it reads the same configuration) after setting
up, and after changes.
