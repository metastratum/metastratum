# Hosts

Hosts enroll with a single-use join token (Infrastructure → Hosts, or
`mst-controller join-token`) and get a certificate from the controller's CA,
valid for 30 days. Agents renew it when a third of that is left, or on
demand (Renew certificate on the host). A renewal revokes any other
certificate the host held but did not use, so it never holds more than two.
`mst_host_cert_expiry_timestamp_seconds` shows how long each has.

What a host reports about itself is taken as it is when it enrolls. Later
changes that would give it more (another name, address or OVN chassis, a
physical network it did not have, more CPUs or memory) wait until a
provider administrator accepts them (Manage on the host, "Changes
waiting"); until then the host keeps what it had. A host taken over cannot
so make itself the gateway of other tenants' VPCs, or draw their VMs.
Less of anything is taken at once.

Hosts' libvirt does not listen on the network: the controller drives live
migrations on each host's own libvirt, and QEMU sends the VM over TLS
(migration ports 49152-49215 must be open between hosts; disks moving
between hosts use them too, see [Disks](disks.md)). Each migration
gets certificates of its own from a CA the controller makes for it, so the
destination accepts the VM from that source only. The agent puts them in
`/etc/pki/qemu` just while QEMU loads them, and removes them after; the
host's own certificate is never there (a guest breaking out into QEMU
could otherwise act as the host). The destination defines the incoming VM
from the controller's description of it, never from what the source
sends, but where its devices sit. A host shows "No migration" when the agent finds no group QEMU runs
in (`libvirt-qemu`, `qemu` or `kvm`) to give those keys to.

A renewed certificate's predecessor is revoked as soon as the host has
connected with the new one; controllers refuse revoked certificates.

To retire a machine, move its VMs off (Evacuate), then decommission it
(Manage on the host). Evacuating also copies the images the host holds
the only copy of to other hosts: images are not downloaded again, so they
would go with it. Decommissioning a connected host that still holds one is
refused; a host that is down took them along (the report names them;
delete them and add them again). Either way the host leaves its cluster and
its certificates are revoked, and the dialog asks for the host's name:

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

## CPUs and clusters

A cluster is where VMs move between hosts, so its hosts should have CPUs
of one vendor: VMs do not move between AMD and Intel (a host is not moved
into a cluster of the other vendor). A VM that may migrate gets a CPU
model every host of its cluster runs as it is made: libvirt's baseline
of their CPUs, without features that keep a VM from migrating. It keeps
that model for life. Where there is none (CPUs of both vendors), the VM is
made non-migratable and gets its host's CPU, rather than a model that
only seems to move.

The controller knows each host's CPU and each VM's model as lists of
features, and moves and restarts a VM only on hosts that have every
feature of its model (a VM with its host's CPU: every feature of that
host). A host that joins with less (an older CPU) runs the VMs made after
it joined; Infrastructure shows which VMs of its cluster it cannot run.
Fit those to the cluster (Details → CPU → Fit to cluster, while stopped):
they get the model all hosts run now, from their next start, and may see
fewer CPU features.

A VM can also be made non-migratable, as it is created or later while it
is stopped (from its next start): it gets its host's CPU as it is, its
disks use the host's page cache, and it stays on its host (evacuating the
host leaves it there). That is the default in a cluster of one host.

## Limits

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
