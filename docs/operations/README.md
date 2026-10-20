# Running metastratum

How to run metastratum beyond a single dev machine: the pieces, controllers in
more than one place, the databases underneath, what happens when a host
dies, upgrades, and monitoring. [Development](../development.md) covers building
and the lab; [SECURITY.md](../../SECURITY.md) covers the security model.

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

## The control plane

- [Controllers](controllers.md): several replicas, behind a load balancer; checking an installation
- [PostgreSQL](postgresql.md): the database: what it must offer (advisory locks, LISTEN/NOTIFY), HA with failover
- [OVN](ovn.md): the virtual networks' databases, over TLS
- [Upgrades](upgrades.md): rolling upgrades of controllers and agents
- [Monitoring](monitoring.md): Prometheus metrics, alerts, logs
- [Rate limits](rate-limits.md): API rate limits, alone or shared through Redis
- [Two-factor sign-in](two-factor.md): authenticator apps, passkeys and security keys, YubiKey OTP

## Hosts and storage

- [Hosts](hosts.md): enrolling, certificates, migration TLS, retiring; CPUs and clusters; limits
- [When a host fails](host-failures.md): HA: detection, fencing, restarting VMs elsewhere
- [Deleting pools and images](storage.md): removing storage pools and images, and what stays on the storage
- [Images](images.md): adding images, downloaded or uploaded; copies on hosts
- [Windows images](windows.md): building Windows Server and Windows 11 images; accounts and passwords

## VMs

- [Resizing](resizing.md): CPUs, memory and disks, running and stopped; room on hosts
- [Disks](disks.md): extra disks: attaching, detaching, moving between VMs and pools
- [Network interfaces](network-interfaces.md): adding, moving and removing a VM's interfaces
- [Secure Boot and TPM](boot-security.md): Secure Boot, TPMs, and where a TPM's state lives
- [Sleep](sleep.md): guests that sleep when idle, waking them, and VMs that may not
- [Templates and clones](templates.md): templates, and VMs cloned from them
- [Snapshots](snapshots.md): disk snapshots, and reverting to them
- [Backups](backups.md): backup repositories, incremental backups, schedules, restores
- [Load balancers](load-balancers.md): load balancers in VPCs, done by OVN; health checks
