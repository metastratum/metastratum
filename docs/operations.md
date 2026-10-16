# Running hypermon

How to run hypermon beyond a single dev machine: the pieces, controllers in
more than one place, the databases underneath, what happens when a host
dies, upgrades, and monitoring. The [README](../README.md) covers building
and the lab; [SECURITY.md](../SECURITY.md) covers the security model.

## The pieces

| Piece | Runs on | State | If it is down |
|---|---|---|---|
| `hm-controller` | 1 or more machines | none (all in Postgres) | another replica takes over |
| Redis (optional) | anywhere the controllers reach | rate limit budgets | each replica limits on its own |
| PostgreSQL | its own HA setup | everything hypermon knows | API and tasks stop; VMs keep running |
| OVN central (NB/SB, `ovn-northd`) | 3 machines, Raft | the virtual networks | no network changes; traffic keeps flowing |
| `hm-agent`, libvirt, OVS, `ovn-controller` | every host | none of its own | that host's VMs are unmanaged (and, with HA, restarted elsewhere) |

Nothing on the data path depends on a controller: VMs keep running and
their traffic keeps flowing while every controller is down.

## Controllers

Run as many replicas as you like (two or three is plenty), each with the
same database, master key and OVN remotes. `deploy/systemd/hm-controller.service`
is a unit for them; settings go in `/etc/hypermon/controller.env`:

```sh
DATABASE_URL=postgres://hypermon:...@db.example.net/hypermon?sslmode=verify-full
HM_MASTER_KEY_FILE=/run/credentials/hm-controller.service/master.key
HM_LISTEN=127.0.0.1:8080              # behind the HTTPS proxy
HM_PUBLIC_URL=https://cloud.example.net
HM_SECURE_COOKIES=true
HM_TRUSTED_PROXIES=127.0.0.1
HM_AGENT_LISTEN=0.0.0.0:8443
HM_AGENT_NAMES=ctl1.example.net,10.0.0.21
HM_PEER_ADDRESS=10.0.0.21:8443
HM_OVN_NB=ssl:10.0.0.31:6641,ssl:10.0.0.32:6641,ssl:10.0.0.33:6641
HM_OVN_CA_CERT=/etc/hypermon/ovn/ca.pem
HM_OVN_CERTIFICATE=/etc/hypermon/ovn/controller.pem
HM_OVN_PRIVATE_KEY=/run/credentials/hm-controller.service/ovn-key.pem
HM_OVN_SB=ssl:10.0.0.31:6642,ssl:10.0.0.32:6642,ssl:10.0.0.33:6642
HM_METRICS_LISTEN=10.0.0.21:9180
HM_REDIS_URL=rediss://:...@redis.example.net:6380
```

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
- set `X-Forwarded-For`, and be listed in `--trusted-proxies` so rate
  limits see clients rather than the proxy;
- reach `/healthz`, which answers 200 while the replica can reach Postgres.

**Agent traffic** does not go through that proxy. Each host dials a
replica's agent endpoint (`--agent-listen`, gRPC with mutual TLS against
the controller's own CA) and keeps one session open. Give the agents every
replica, comma-separated, in `HM_CONTROLLER` in `/etc/hypermon/agent.env`
(`https://ctl1.example.net:8443,https://ctl2.example.net:8443`); an agent
that loses its replica moves to the next. `--agent-names` must include the
name or address agents use for that replica: the endpoint's certificate is
issued for those names.

A request or task may land on a replica other than the one holding the
host's session. It then forwards the command, or console stream, to that
replica over the agent endpoint, so replicas must reach each other's
`--peer-address`, and authenticate each other with certificates the CA
issues them at startup.

**Work shared between replicas.** Tasks (creating, migrating VMs...) are
claimed from Postgres by any replica with a lease, and resumed by another
if their replica dies. Network reconciliation runs on whichever replica
gets a change first, serialized by a database lock. Watching hosts for
failure (below) runs on one replica at a time, the leader, chosen with a
Postgres advisory lock that is released when its session ends.

## PostgreSQL

hypermon needs one writable PostgreSQL database (tested with 17 and 18),
and relies on three things that rule out some setups:

- **Session-level advisory locks** (leader election) and **LISTEN/NOTIFY**
  (live updates): connection poolers in transaction mode (PgBouncer's
  `pool_mode = transaction`) break both. Connect directly, or through a
  proxy that keeps a client on one server connection for good (HAProxy,
  PgBouncer in session mode).
- **Read-your-writes**: everything goes to the primary; there is no use for
  read replicas.
- About 20 connections per controller replica.

A recipe that fits: three PostgreSQL servers managed by
[Patroni](https://patroni.readthedocs.io/) (with etcd, or the
Kubernetes API, as its store), synchronous replication to one standby
(`synchronous_mode: true`) so a failover loses no committed task, and
HAProxy in front sending connections to whichever node Patroni's REST API
reports as primary (`option httpchk GET /primary`). Point `DATABASE_URL` at
HAProxy.

On failover the controllers' connections break. They reconnect on their
own: tasks that were mid-step resume from their last recorded step, the
leader loses its lock and one is elected again, and the live-update
listener reconnects and tells browsers to refresh. Expect API errors for
the seconds the failover takes, and nothing else.

Back up the database as usual (`pg_basebackup` plus WAL archiving, or
pgBackRest). A restore rolls hypermon back to that moment: VMs created
since are unknown to it (their domains are left alone on the hosts, but
will need cleaning up by hand).

## OVN

Run the northbound and southbound databases as three-server Raft clusters
with `ovn-northd` beside them (`ovn-ctl --db-nb-cluster-local-addr ...`).
Give the controllers all three northbound servers; they use the leader and
follow it when it changes.

Use TLS for both: whoever can write the northbound database can rewrite
every tenant's network, and whoever can write the southbound one, the flows
on every host.

**Northbound**, which only the controllers use: create a PKI with
`ovs-pki` (or any CA), give the servers a certificate
(`ovn-nbctl set-ssl <key> <cert> <ca>`, `ovn-nbctl set-connection
pssl:6641`), and the controllers the CA and a client certificate
(`--ovn-ca-cert`, `--ovn-certificate`, `--ovn-private-key`). Keep this PKI
apart from the southbound's: hosts must never reach the northbound
database.

**Southbound**, which every host's `ovn-controller` uses, hypermon sets up
itself. The controller keeps a CA for it (separate from the one for agents
and replicas), and with `--ovn-sb ssl:...` each agent gets a certificate
from it naming its chassis, installs it (`ovs-vsctl set-ssl`), points
`ovn-controller` at the southbound remote, and renews the certificate like
its own. OVN's RBAC then holds each host to its own chassis: a host that is
taken over cannot rewrite the others' bindings or flows. The chassis name is
the host's Open vSwitch `system-id`; two live hosts cannot have the same
one. On the southbound servers:

```sh
hm-controller ovn-certificate --name sb1 --out-dir /tmp/sb1   # on a controller
ovn-sbctl set-ssl /etc/ovn/sb1-key.pem /etc/ovn/sb1.pem /etc/ovn/ca.pem
ovn-sbctl set-connection role=ovn-controller pssl:6642
```

To move hosts over without interruption, add the TLS listener beside the
plain one first (as `deploy/lab/lab.sh ovn-sb-tls` does on the lab), start
the controllers with `--ovn-sb`, and remove the plain listener once every
host has switched (`ovn-sbctl list Connection`, and each host's
`external_ids:ovn-remote`).

`deploy/lab/lab.sh ovn-tls` and `ovn-sb-tls` set up both on the lab.

## Hosts

Hosts enroll with a single-use join token (Infrastructure → Hosts, or
`hm-controller join-token`) and get a certificate from the controller's CA,
valid for 30 days. Agents renew it when a third of that is left, or on
demand (Renew certificate on the host), and libvirt picks up the new one
without restarting.

Hosts' libvirt does not listen on the network: the controller drives live
migrations on each host's own libvirt, and QEMU sends the VM over TLS
(migration ports 49152-49215 must be open between hosts). The agent closes
libvirt's TLS listener, and opens it only while a host in its cluster runs
an agent from before protocol 3, which migrates peer to peer; it then also
limits it to host certificates (`tls_allowed_dn_list`). Either change
restarts libvirt once, after migrations on the host have ended; running VMs
do not notice. `hm_host_cert_expiry_timestamp_seconds` shows how long
each has.

A renewed certificate's predecessor is revoked as soon as the host has
connected with the new one. Hosts install the CA's revocation list for
QEMU and libvirt (`/etc/pki/qemu/ca-crl.pem`, `/etc/pki/CA/cacrl.pem`); the
controller sends a new one whenever it revokes something, and with every
welcome.

To retire a machine, move its VMs off and Decommission it (Manage on the
host). It leaves its cluster, its certificates are revoked, and its agent
can no longer connect. To use the machine again, enroll it anew. Its OVN
chassis stays in the southbound database until you delete it
(`ovn-sbctl chassis-del <name>`).

### Limits

VMs on one host share its disks and network; VMs on shared storage share
it too. Set per-VM limits on a cluster (Infrastructure → Clusters) to keep
one from starving the rest:

- network, in Mbit/s per NIC and each way: enforced by OVN, as QoS meters
  on the VM's port (traffic above it is dropped);
- disk operations and throughput per disk: enforced by QEMU (libvirt's
  `<iotune>`).

Changes apply at once to running VMs, as well as to new ones.

Put a host in maintenance before working on it; Evacuate also live-migrates
its running VMs to other hosts in its cluster. Stopped VMs and VMs that are
not migratable stay where they are.

### When a host fails

With HA enabled on its cluster (Infrastructure → Clusters), the leader
watches the cluster's hosts. A host has failed when its agent has been
silent for 60 seconds **and** it does not answer on its libvirt TLS or SSH
ports either (a crashed agent on a healthy host is not a failed host), and
at most half of its cluster looks failed (more is likelier a problem on the
controller's side of the network).

The leader then **fences** the host through a channel independent of its
operating system, configured per host (Manage → Fencing):

- `redfish`: the host's BMC (iDRAC, iLO, XClarity, OpenBMC...): `ForceOff`,
  then waits for the BMC to report it off;
- `libvirt`: for hosts that are themselves VMs, as in the lab.

Only once fencing is confirmed does a recovery task restart the host's VMs
on other hosts of the cluster, for VMs whose disks are all on shared
storage (NFS, Ceph). VMs on the host's local disks wait for it. **Without
fencing configured nothing is restarted**: a host that only lost its
network may still be running its VMs, and starting them a second time on
the same disks corrupts them. "Test" on the fencing form checks the BMC
answers, without powering anything off.

A fenced host stays `fenced`, taking no VMs, until you bring it back:
repair it and power it on; its agent reconnects by itself, and removes the
domains of VMs that were restarted elsewhere. Then Manage → Save puts it
back in service.

## Upgrades

Controllers and agents say which protocol revision they speak and which
commands an agent handles. Upgrade **controllers first, then agents**:

1. Upgrade the controllers one at a time. A new controller serves older
   agents (anything from the oldest revision it supports; it refuses older
   ones with a message naming both), and does not send them commands they
   do not handle; the action fails with "upgrade the agent" instead.
   Database migrations run when the first new controller starts. They are
   kept compatible with the previous release's controllers (adding to the
   schema, and only removing what that release no longer uses), and older
   replicas ignore migrations they do not know, so they keep working, and
   can restart, until they are replaced.
2. Upgrade the agents, a host at a time or all at once. Restarting an agent
   does not touch its VMs. Hosts can be drained to upgraded hosts
   throughout: while a cluster mixes agents that migrate peer to peer with
   newer ones, the newer ones keep libvirt's TLS listener open for them,
   and close it once the last host is upgraded.

The release notes say when a release drops support for an agent revision.

## Monitoring

`--metrics-listen` serves Prometheus metrics at `/metrics`, on an address of
its own rather than the API's: they name hosts and count resources across
tenants. Scrape every replica:

```yaml
scrape_configs:
  - job_name: hypermon
    static_configs:
      - targets: ['ctl1.example.net:9180', 'ctl2.example.net:9180']
```

Counters (`hm_*_total`, histograms `hm_*_seconds`) are what that replica
did; sum them over replicas. Gauges about the platform (`hm_vms`,
`hm_host_up`, `hm_tasks_open`, ...) are read from the database, so every
replica reports the same values: use `max`.

| Metric | What |
|---|---|
| `hm_http_requests_total{method,route,status}`, `hm_http_request_duration_seconds` | API requests |
| `hm_tasks_finished_total{kind,outcome}`, `hm_task_duration_seconds{kind}` | tasks run |
| `hm_tasks_open{kind,state}` | tasks waiting or running |
| `hm_agent_commands_total{command,outcome}`, `hm_agent_command_duration_seconds` | commands sent to agents |
| `hm_agent_sessions` | agent sessions on this replica |
| `hm_host_up{host,state}` | whether each host's agent is connected and heartbeating |
| `hm_host_cert_expiry_timestamp_seconds{host}`, `hm_host_last_seen_timestamp_seconds{host}` | |
| `hm_vms{status,power}`, `hm_migrations_running` | |
| `hm_network_passes_total{outcome}`, `hm_network_changes_total{op}`, `hm_network_pass_duration_seconds` | OVN reconciliation |
| `hm_logins_total{method,outcome}`, `hm_rate_limited_total`, `hm_rate_limit_store_errors_total` | |
| `hm_fencings_total{outcome}`, `hm_leader`, `hm_replicas` | HA |

Alerts worth having:

```yaml
groups:
  - name: hypermon
    rules:
      - alert: HypermonHostDown
        expr: max by (host) (hm_host_up{state=~"ready|maintenance"}) == 0
        for: 2m
      - alert: HypermonNoLeader
        expr: sum(hm_leader) == 0
        for: 2m
      - alert: HypermonNetworkReconcileFailing
        expr: sum(increase(hm_network_passes_total{outcome="failed"}[10m])) > 3
      - alert: HypermonFencingFailed
        expr: sum(increase(hm_fencings_total{outcome="failed"}[15m])) > 0
      - alert: HypermonHostCertificateExpiring
        expr: min by (host) (hm_host_cert_expiry_timestamp_seconds) - time() < 30 * 86400
      - alert: HypermonTasksPiling
        expr: sum(max by (kind) (hm_tasks_open{state="pending"})) > 50
        for: 10m
      - alert: HypermonLoginFailures
        expr: sum(rate(hm_logins_total{outcome="failed"}[5m])) > 1
        for: 10m
```

Logs go to stderr (journald under systemd); `RUST_LOG` sets the level, e.g.
`RUST_LOG=info,hm_controller=debug`.

## Rate limits

The API limits requests per client address (50 per second, bursts of 300),
password logins per address (one per 2 seconds, bursts of 30), and failed
logins per account (5, then one a minute). An account under attack
therefore cannot be guessed at quickly, but its user is slowed down, not
locked out: the budget refills. Refusals are HTTP 429 with `Retry-After`,
counted in `hm_rate_limited_total`.

Give the controllers a Redis (`--redis-url`, `rediss://` for TLS) and they
share these budgets, whichever replica a client reaches, and keep them
across restarts. Redis holds nothing else: losing it loses nothing but the
budgets, and while it cannot be reached each replica limits on its own
(`hm_rate_limit_store_errors_total` counts the misses). Without Redis,
limits are per replica, so behind a load balancer spreading clients over N
replicas a client gets up to N times the budget. Account names are hashed
before they are used as keys.
