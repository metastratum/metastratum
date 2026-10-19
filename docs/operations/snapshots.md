# Snapshots

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
