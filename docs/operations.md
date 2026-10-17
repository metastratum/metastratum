# Running metastratum

How to run metastratum beyond a single dev machine: the pieces, controllers in
more than one place, the databases underneath, what happens when a host
dies, upgrades, and monitoring. The [README](../README.md) covers building
and the lab; [SECURITY.md](../SECURITY.md) covers the security model.

## The pieces

| Piece | Runs on | State | If it is down |
|---|---|---|---|
| `mst-controller` | 1 or more machines | none (all in Postgres) | another replica takes over |
| Redis (optional) | anywhere the controllers reach | rate limit budgets | each replica limits on its own |
| PostgreSQL | its own HA setup | everything metastratum knows | API and tasks stop; VMs keep running |
| OVN central (NB/SB, `ovn-northd`) | 3 machines, Raft | the virtual networks | no network changes; traffic keeps flowing |
| `mst-agent`, libvirt, OVS, `ovn-controller` | every host | none of its own | that host's VMs are unmanaged (and, with HA, restarted elsewhere) |

Nothing on the data path depends on a controller: VMs keep running and
their traffic keeps flowing while every controller is down.

## Controllers

Run as many replicas as you like (two or three is plenty), each with the
same database, master key and OVN servers. `deploy/systemd/mst-controller.service`
is a unit for them. Their settings are in `/etc/metastratum/controller.toml`,
with the master key beside it as `master.key`; the unit passes both in as
credentials, readable by the controller alone. A typical file:

```toml
database_url = "postgres://metastratum:...@db.example.net/metastratum?sslmode=verify-full"
metrics_listen = "10.0.0.21:9180"

[api]
trusted_proxies = ["10.0.0.5", "10.0.0.6"]
redis_url = "rediss://:...@redis.example.net:6380"

[ovn]
northbound = ["ssl:10.0.0.31:6641", "ssl:10.0.0.32:6641", "ssl:10.0.0.33:6641"]
southbound = ["ssl:10.0.0.31:6642", "ssl:10.0.0.32:6642", "ssl:10.0.0.33:6642"]
```

Everything else has a default: the API on `127.0.0.1:8080` (for the proxy
in front), the agent endpoint on port 8443. `deploy/config/controller.toml`
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
(`controllers = ["https://ctl1.example.net:8443", "https://ctl2.example.net:8443"]`,
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

### Checking an installation

`mst-controller doctor` looks at a running installation against the
deployment checklist in [SECURITY.md](../SECURITY.md#deployment-checklist):

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

## PostgreSQL

metastratum needs one writable PostgreSQL database (tested with 17 and 18),
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
reports as primary (`option httpchk GET /primary`). Point `database_url` at
HAProxy.

On failover the controllers' connections break. They reconnect on their
own: tasks that were mid-step resume from their last recorded step, the
leader loses its lock and one is elected again, and the live-update
listener reconnects and tells browsers to refresh. Expect API errors for
the seconds the failover takes, and nothing else.

Back up the database as usual (`pg_basebackup` plus WAL archiving, or
pgBackRest). A restore rolls metastratum back to that moment: VMs created
since are unknown to it (their domains are left alone on the hosts, but
will need cleaning up by hand).

## OVN

Run the northbound and southbound databases as three-server Raft clusters
with `ovn-northd` beside them (`ovn-ctl --db-nb-cluster-local-addr ...`).
Give the controllers all three northbound servers; they use the leader and
follow it when it changes.

Use TLS for both: whoever can write the northbound database can rewrite
every tenant's network, and whoever can write the southbound one, the flows
on every host. The controller has the certificate authorities for both:

- **Northbound**, which only the controllers use, trusts a CA of its own
  that issues certificates to controllers only: the northbound database has
  no access control, so hosts must never get in. Each replica issues itself
  a client certificate from it at startup and renews it.
- **Southbound**, which every host's `ovn-controller` uses, trusts the OVN
  CA. With `ovn.southbound` set, each agent gets a certificate from it naming
  its chassis, installs it, points `ovn-controller` at the southbound remote,
  and renews it like its own. OVN's RBAC then holds each host to its own
  chassis: a host that is taken over cannot rewrite the others' bindings or
  flows. The chassis name is the host's Open vSwitch `system-id`; two live
  hosts cannot have the same one.

The OVN servers' certificates come as one bundle per OVN central node:

```sh
mst-controller ovn-certificate --name ovn1 --address 10.0.0.31 --out-dir ovn1   # on a controller
scp -r ovn1 root@10.0.0.31: && ssh root@10.0.0.31 sh ovn1/setup.sh
```

`setup.sh` installs the certificates in `/etc/ovn/metastratum`, configures
both databases to use them, and serves them over TLS: northbound on
`pssl:6641`, southbound on `pssl:6642` with OVN's RBAC (`--nb-port` and
`--sb-port` choose others), replacing plain listeners on those ports. In a
cluster the settings are shared, so the files must be at the same place on
every node: run each node's bundle on that node. Then start the controllers
with `ssl:` servers in `ovn.northbound` and `ovn.southbound`, nothing else.

The server certificates last a year (`--days`). The controllers look at
what each server presents every hour, export its expiry
(`mst_ovn_certificate_expiry_timestamp_seconds`), and warn in their log a
month ahead, or when a server presents a certificate from another CA. To
renew, make a new bundle and run its `setup.sh` again: the servers pick the
new files up without restarting.

`deploy/lab/lab.sh ovn-tls` installs the lab's bundle on `mst-ctl`.

## Hosts

Hosts enroll with a single-use join token (Infrastructure → Hosts, or
`mst-controller join-token`) and get a certificate from the controller's CA,
valid for 30 days. Agents renew it when a third of that is left, or on
demand (Renew certificate on the host), and libvirt picks up the new one
without restarting. A renewal revokes any other certificate the host held
but did not use, so it never holds more than two.

What a host reports about itself is taken as it is when it enrolls. Later
changes that would give it more (another name, address or OVN chassis, a
physical network it did not have, more CPUs or memory) wait until a
provider administrator accepts them (Manage on the host, "Changes
waiting"); until then the host keeps what it had. A host taken over cannot
so make itself the gateway of other tenants' VPCs, or draw their VMs.
Less of anything is taken at once.

Hosts' libvirt does not listen on the network: the controller drives live
migrations on each host's own libvirt, and QEMU sends the VM over TLS
(migration ports 49152-49215 must be open between hosts). Each migration
gets certificates of its own from a CA the controller makes for it, so the
destination accepts the VM from that source only. The agent puts them in
`/etc/pki/qemu` just while QEMU loads them, and the host's own back after.
The destination defines the incoming VM from the controller's description
of it, never from what the source sends. Agents older than protocol 4 (the
first release's) are refused, and the agent closes libvirt's TLS listener
should an older one have left it open. Either change
restarts libvirt once, after migrations on the host have ended; running VMs
do not notice. `mst_host_cert_expiry_timestamp_seconds` shows how long
each has.

A renewed certificate's predecessor is revoked as soon as the host has
connected with the new one. Hosts install the CA's revocation list for
QEMU and libvirt (`/etc/pki/qemu/ca-crl.pem`, `/etc/pki/CA/cacrl.pem`); the
controller sends a new one whenever it revokes something, and with every
welcome.

To retire a machine, move its VMs off (Evacuate), then decommission it
(Manage on the host). Either way the host leaves its cluster and its
certificates are revoked, and the dialog asks for the host's name:

- **Unlink** (the default) retires it in metastratum and leaves the machine as
  it is. Its agent keeps running but can no longer connect. To enroll the
  machine again, remove the agent's identity (`/var/lib/metastratum-agent`)
  and enroll it anew.
- **Delete** also removes metastratum from the machine, if its agent is
  connected: it unmounts storage pools, removes libvirt secrets, cached
  images, local volumes, cloud-init seeds and metastratum's TLS files, removes
  its own identity, and stops and disables itself. To enroll the machine
  again, just enroll it anew.

OVN is yours, so it stays either way: `ovn-controller` keeps running, and
the chassis, its bridges and settings stay. Delete only undoes what
metastratum changed: it puts back the southbound address the host had before
`ovn.southbound` (the agent keeps a record of it) and removes metastratum's TLS
settings. Delete the chassis yourself (`ovn-sbctl chassis-del <name>`) if
the machine leaves OVN too. A re-enrolled machine keeps its name; the old
record stays, listed as decommissioned.

### Deleting pools and images

A storage pool that holds no VM disks can be removed two ways:

- **Unlink** (the default): every host unmounts it and forgets its
  credentials; the storage is left as it is, cached images included.
  Adding the pool again later finds them there.
- **Delete**: also removes metastratum's data from the storage: its cached
  images, and volumes if any were left (on NFS, `images/` and `volumes/`
  files named after metastratum IDs; on Ceph, `img-*` images). Other files
  there stay. If another pool uses the same storage (the same NFS export,
  or the same Ceph pool), the data stays: that pool needs it.

An image is refused when it describes a disk larger than the one it is for
(checked before it is unpacked: a small download can unpack into a huge
disk). Copies of tenants' images are dropped from hosts once no VM disk is
built on them any more; the provider's stay cached.

Deleting an image (no VM disk may be built on it) removes its cached copy
from every host and every shared pool. Hosts that are down at the time
clean up when they reconnect. The outcome is in the audit log
(`storage_pool.cleanup`, `image.cleanup`).

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
storage (NFS, Ceph). VMs on the host's local disks wait for it. Only VMs
meant to run are restarted: not those stopped through the API, nor those
shut down or powered off from inside the guest (or with `virsh` on the
host), which count as stopped from then on. A VM that crashed is still
meant to run. **Without
fencing configured nothing is restarted**: a host that only lost its
network may still be running its VMs, and starting them a second time on
the same disks corrupts them. "Test" on the fencing form checks the BMC
answers, without powering anything off.

A fenced host stays `fenced`, taking no VMs, until you bring it back:
repair it and power it on; its agent reconnects by itself, and removes the
domains of VMs that were restarted elsewhere. Then Manage → Save puts it
back in service.

## Load balancers

A load balancer is an address whose ports spread connections over member
VMs (Networks → Load balancers). OVN does it in the VPC's router and
switches: there are no appliance VMs, and nothing extra to run.
It works at the transport layer: TCP and UDP listeners, each forwarding a
port to a port on the members; TLS and HTTP routing are up to the members.

- **External**: the address comes from the VPC's external network, and is
  reachable from outside like a floating IP. The VPC keeps its external
  network while it has external load balancers.
- **Internal**: the address is in one of the VPC's networks, for its VMs.

Connections keep the client's address, so the members' security groups
decide who gets through (guardrails too). With health checks (the default)
OVN connects to each member's port every few seconds and leaves out those
that do not answer; the checks come from an address reserved in each
member's network (its highest free one), and OVN lets them past security
groups and guardrails (by its own MAC address, so nothing else from that
address gets through). Load balancers count against the tenant's
`load_balancers` quota.

## Resizing

Resize a VM from its page (Resize). CPUs and memory beyond what they may
grow to while it runs (below) change while it is stopped, and only as far
as its host has room within the cluster's overcommit; it gets them at its
next start. Disks only grow, running or
not: a running guest sees the larger disk at once, and cloud images grow
their partition and root file system at the next boot (cloud-init's
growpart); otherwise grow them in the guest (`growpart`, `resize2fs`).
qcow2 disks (local and NFS storage) cannot grow while they hold
snapshots, so delete the VM's snapshots first; Ceph disks can. What grows
counts against the tenant's quotas.

### CPUs and memory while a VM runs

A VM can be given room to grow ("CPUs can grow to" and "Memory can grow
to" when it is made, or Resize while it is stopped). Its CPUs and memory
then change while it runs, anywhere up to there, at once and without a
restart:

- **CPUs** are plugged in and out. Linux leaves CPUs plugged in while it
  runs offline until told otherwise; VMs made with metastratum's own
  cloud-init get a udev rule that brings them online as they arrive.
  With user data of your own, add it yourself:

  ```
  # /etc/udev/rules.d/80-metastratum-cpu-hotplug.rules
  SUBSYSTEM=="cpu", ACTION=="add", TEST=="online", ATTR{online}=="0", ATTR{online}="1"
  ```

- **Memory** goes through the VM's virtio balloon: the guest gives it back,
  or gets it. That needs the guest's balloon driver (`virtio_balloon`, in
  every common Linux kernel and in the virtio drivers for Windows); the VM
  page shows whether it reports, and what the guest uses of its memory.

The host keeps the maximums for the VM, and they count against the
tenant's `vcpus` and `memory_mib` quotas, so growing within them never
fails for want of room. Raising a maximum itself needs the VM stopped.

Guests also hand memory they free back to their host as they free it
(free page reporting), so a host's actual use stays below what it keeps
for its VMs. It can be turned off per VM (Details → Free memory, or
`free_page_reporting` in the API and Terraform), for a guest that should
keep its memory: one that frees and reuses a lot of it quickly pays for
getting it back from the host each time. It is part of the VM's balloon,
so it changes while the VM is stopped and applies from its next start.
VMs made before it existed get it at their next start from stopped.

## Templates and clones

Clone a stopped VM from its page (Clone): the new VM gets copies of its
disks, its CPUs, memory and firmware, in the same tenant and storage, on a
new network interface. It boots as a new machine: cloud-init gives it its
own network, host name and SSH host keys, and the password or keys given
when cloning; the disks keep everything else. On local storage the clone
is made on the same host; on NFS and Ceph anywhere the cluster has room.
Copies on local and NFS storage keep sharing the image the original was
made from, so they take only what differs from it; Ceph copies are whole.

Make a stopped VM a **template** (Details → Template) to keep it for
cloning: a template is never started, and Create VM offers it next to the
images. Prepare it first: install what its clones need, then clear what
must differ between them (`sudo cloud-init clean --machine-id`), and power
it off.

While a clone is being made, its source is not started, deleted,
snapshotted or backed up.

## Snapshots

A snapshot keeps a VM's disks as they are, to put them back to later: before
an upgrade, say. Unlike a backup it is kept in the disks themselves (qcow2
internal snapshots on local and NFS storage, RBD snapshots on Ceph), so it
is quick to take and to go back to, takes only the space of what changes
afterwards, and goes with its VM when the VM is deleted. It is not a backup:
it lives on the same storage as the VM.

Take one from the VM's page (Details → Snapshots), running or stopped; a VM
has at most 16. A running VM's disks are all taken at the same moment, by
its QEMU, while it carries on; with the QEMU guest agent its file systems
are frozen for it and the snapshot is **consistent**, otherwise it is
**crash-consistent**, as for backups. (libvirt notes a VM snapshotted while
running as tainted, `custom-monitor`, in its log: metastratum asks QEMU
directly, as libvirt has no call for these snapshots.)

Reverting puts the disks back as they were in the snapshot, and needs the
VM stopped: what was written since is lost, the VM's other snapshots stay.
Its memory is not part of a snapshot; the VM boots afresh.

Each snapshot counts against the `snapshot_gib` quota with the size of the
VM's disks when it was taken: as much as it can grow to.

VMs with snapshots on their host's local disk cannot be live-migrated (nor
evacuated): a block migration copies the disks' data, not their snapshots.
Delete the snapshots first. On NFS and Ceph they migrate with the VM.

## Backups

A backup is a copy of a VM's disks, kept in a **backup repository** apart
from the storage the VM runs on. It outlives the VM: restoring it makes a
new VM, also after the original is deleted.

Repositories are NFS exports (Infrastructure → Backups). Hosts mount them
as they do NFS storage pools, so every host needs to reach the export, and
it needs `no_root_squash`: libvirt writes and owns the files as root. Each
backup is a directory, `backups/<backup id>/`, with a qcow2 file per disk;
only metastratum's own directories are touched. Grant a repository to the
tenants that may use it (Grants on a tenant); in a single-tenant
installation, repositories are there for everyone.

Back a VM up from its page (Details → Backups), or give it a schedule:
daily or weekly, keeping the last so many. The leader starts scheduled
backups, and deletes those beyond what the schedule keeps; backups taken by
hand are kept until deleted. A VM is backed up one backup at a time, and
not while it migrates.

- A **running** VM is backed up as it was when the backup started, while
  it carries on (libvirt's backup job). If the VM runs the QEMU guest agent
  (`qemu-guest-agent`), its file systems are frozen for that moment, and the
  backup is **consistent**. Without it, the backup is **crash-consistent**:
  the disks as after a sudden power loss, which journaling file systems and
  databases recover from, but anything still in the guest's memory is
  missing.
- A **stopped** VM's disks are copied as they are, and its backup is
  consistent.

Backups count against the `backup_gib` quota with their disks' full size.
What they take in the repository is usually less (shown with each backup):
only the disks' data is copied, not their empty space.

Restoring a backup creates a new VM with the backup's disks, CPUs and
memory, on a new network interface (a backup can be restored more than
once, and its original may still be running), in the same tenant. It boots
as a new instance: cloud-init sets up its network, host keys and the
password or SSH keys given when restoring; the disks keep everything else.
The restored VM's disks are full copies, not linked to the image the
original was made from.

Deleting a repository needs it empty: delete its backups first. Hosts then
unmount it; its files stay on the export.

## Two-factor sign-in

People with a password can add second factors on their account page, and
signing in then takes one of them as well:

- **Authenticator apps** (TOTP: Google Authenticator, 1Password, Aegis…),
  from a QR code.
- **Security keys and passkeys** over WebAuthn: FIDO2 keys, older FIDO U2F
  keys, and platform passkeys. A key works at the address it was added at
  (`mst.example.com`), since browsers bind it there; browsers use keys only
  at a host name over HTTPS, or at `localhost`.
- **YubiKey OTPs**: the one-time passwords a YubiKey types when touched,
  where the controllers can reach a Yubico validation service. Give them
  its client ID and API key (the `[yubico]` section; YubiCloud by
  default, or `servers` for your own). A YubiKey also works as a security
  key, which needs no service.
- **Backup codes**: ten single-use codes, shown once when the first factor
  is added and replaceable from the account page.

A tenant's administrators can require a second factor of its password
accounts and those of its sub-tenants (Tenants → Sign-in); a sub-tenant
cannot turn off what a tenant above it requires. Someone without one can,
once signed in, only add one. People who sign in through an identity
provider use its second factors instead, and API tokens sign in alone.

Someone who lost theirs is reset by an administrator of their tenant
(Tenants → People → Reset second factor), which signs them out; they then
sign in with their password and add new ones. Where someone else may have
got into the account, revoke its API tokens with the reset (or with a new
password); a plain reset leaves them, so the person's scripts keep
working. For the last provider
administrator, on a controller:

```sh
mst-controller reset-two-factor admin
mst-controller reset-two-factor --tenant acme/dev alice
```

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
   throughout.

The release notes say when a release drops support for an agent revision.

## Monitoring

`metrics_listen` serves Prometheus metrics at `/metrics`, on an address of
its own rather than the API's: they name hosts and count resources across
tenants. Scrape every replica:

```yaml
scrape_configs:
  - job_name: metastratum
    static_configs:
      - targets: ['ctl1.example.net:9180', 'ctl2.example.net:9180']
```

Counters (`mst_*_total`, histograms `mst_*_seconds`) are what that replica
did; sum them over replicas. Gauges about the platform (`mst_vms`,
`mst_host_up`, `mst_tasks_open`, ...) are read from the database, so every
replica reports the same values: use `max`.

| Metric | What |
|---|---|
| `mst_http_requests_total{method,route,status}`, `mst_http_request_duration_seconds` | API requests |
| `mst_tasks_finished_total{kind,outcome}`, `mst_task_duration_seconds{kind}` | tasks run |
| `mst_tasks_open{kind,state}` | tasks waiting or running |
| `mst_agent_commands_total{command,outcome}`, `mst_agent_command_duration_seconds` | commands sent to agents |
| `mst_agent_sessions` | agent sessions on this replica |
| `mst_host_up{host,state}` | whether each host's agent is connected and heartbeating |
| `mst_host_cert_expiry_timestamp_seconds{host}`, `mst_host_last_seen_timestamp_seconds{host}` | |
| `mst_vms{status,power}`, `mst_migrations_running` | |
| `mst_network_passes_total{outcome}`, `mst_network_changes_total{op}`, `mst_network_pass_duration_seconds` | OVN reconciliation |
| `mst_port_binding_mismatches` | VMs' ports OVN has bound to a host other than theirs (leader only; with `ovn.southbound`) |
| `mst_logins_total{method,outcome}`, `mst_rate_limited_total`, `mst_rate_limit_store_errors_total` | |
| `mst_fencings_total{outcome}`, `mst_leader`, `mst_replicas` | HA |

Alerts worth having:

```yaml
groups:
  - name: metastratum
    rules:
      - alert: MetastratumHostDown
        expr: max by (host) (mst_host_up{state=~"ready|maintenance"}) == 0
        for: 2m
      - alert: MetastratumNoLeader
        expr: sum(mst_leader) == 0
        for: 2m
      - alert: MetastratumNetworkReconcileFailing
        expr: sum(increase(mst_network_passes_total{outcome="failed"}[10m])) > 3
      - alert: MetastratumFencingFailed
        expr: sum(increase(mst_fencings_total{outcome="failed"}[15m])) > 0
      - alert: MetastratumHostCertificateExpiring
        expr: min by (host) (mst_host_cert_expiry_timestamp_seconds) - time() < 30 * 86400
      - alert: MetastratumTasksPiling
        expr: sum(max by (kind) (mst_tasks_open{state="pending"})) > 50
        for: 10m
      - alert: MetastratumLoginFailures
        expr: sum(rate(mst_logins_total{outcome="failed"}[5m])) > 1
        for: 10m
      # A host claiming another's VM ports: it may have been taken over.
      # The audit log (network.port_binding_mismatch) says which.
      - alert: MetastratumPortBoundElsewhere
        expr: max(mst_port_binding_mismatches) > 0
        for: 3m
```

Logs go to stderr (journald under systemd); `RUST_LOG` sets the level, e.g.
`RUST_LOG=info,mst_controller=debug`.

## Rate limits

The API limits requests per client address (50 per second, bursts of 300),
password logins per address (one per 2 seconds, bursts of 30), and failed
logins per account (5, then one a minute; wrong second factors and wrong
passwords when changing a second factor count too). An account under attack
therefore cannot be guessed at quickly, but its user is slowed down, not
locked out: the budget refills. Refusals are HTTP 429 with `Retry-After`,
counted in `mst_rate_limited_total`.

Give the controllers a Redis (`api.redis_url`, `rediss://` for TLS) and they
share these budgets, whichever replica a client reaches, and keep them
across restarts. Redis holds nothing else: losing it loses nothing but the
budgets, and while it cannot be reached each replica limits on its own
(`mst_rate_limit_store_errors_total` counts the misses). Without Redis,
limits are per replica, so behind a load balancer spreading clients over N
replicas a client gets up to N times the budget. Account names are hashed
before they are used as keys.
