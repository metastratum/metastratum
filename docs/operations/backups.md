# Backups

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

A backup is **full** (every disk copied whole) or **incremental**: only
what changed since the VM's last backup in the same repository, which it
builds on. Restoring either gives the whole VM. Incremental is the default
for backups taken by hand; a schedule takes a full one every so many
backups (**Full every**, 7 by default; 1 takes them all in full), and
incremental ones in between. A full backup now and then bounds how many
backups a restore reads through, and how much rests on one file.

- A **running** VM's changes are tracked by QEMU: each backup leaves a
  checkpoint (libvirt's, a dirty bitmap in each qcow2 disk) for the next,
  which copies just the blocks written since. The tracking moves with the
  VM when it migrates.
- A **stopped** VM's changes are found by comparing its disks with the last
  backup's copies: the whole of both is read, only the differences stored.
- Disks on **Ceph** are copied in full each time, in incremental backups
  too.

A backup asked to be incremental is taken in full, and says why, when
there is nothing to build on: no earlier backup in the repository, or the
VM's tracking does not reach back to it. That happens after its QEMU was
killed (a host crash), after reverting it to a snapshot, or when its disks
moved to other storage.

An incremental backup's files are qcow2 files backed by the previous
backup's (`../<parent id>/<index>.qcow2`). Deleting a backup that another
builds on merges it into that one first (`qemu-img commit`), which stays
whole: schedules prune the oldest backups this way, and pruning a full one
makes the next one stand alone. Work on one chain of backups goes one at
a time; deleting a backup is refused while one built on it is taken or
restored, and restoring while one it builds on is being merged away. A
deletion that failed half-way leaves the backup `deleting`: delete it
again.

- A **running** VM is backed up as it was when the backup started, while
  it carries on (libvirt's backup job). If the VM runs the QEMU guest agent
  (`qemu-guest-agent`), its file systems are frozen for that moment, and the
  backup is **consistent**. Without it, the backup is **crash-consistent**:
  the disks as after a sudden power loss, which journaling file systems and
  databases recover from, but anything still in the guest's memory is
  missing.
- A **stopped** VM's disks are copied as they are, and its backup is
  consistent.

Backups count against the `backup_gib` quota with what they take in the
repository (shown with each backup): only the disks' data is copied, not
their empty space, and an incremental backup only what changed. While one
is taken, it counts with its disks' full size, as much as it could take.

Restoring a backup creates a new VM with the backup's disks, CPUs and
memory, on a new network interface (a backup can be restored more than
once, and its original may still be running), in the same tenant. It boots
as a new instance: cloud-init sets up its network, host keys and the
password or SSH keys given when restoring; the disks keep everything else.
The restored VM's disks are full copies, not linked to the image the
original was made from.

Deleting a repository needs it empty: delete its backups first. Hosts then
unmount it; its files stay on the export.
