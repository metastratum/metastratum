# Disks

Every VM has its boot disk, and can have up to 16 disks of their own
besides (Machines → Disks, or Add disk on a VM's page). A disk is made
empty, on local storage (on a host) or in a shared pool, and attached to
a VM of its tenant: one that runs has it plugged in at once, and the guest
finds it under `/dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_<disk ID>` (format
it there); a stopped one from its next start. Detaching one from a
running VM unplugs it: unmount it in the guest first. Disks grow like a
VM's (Change), attached or not, and count against the tenant's disk quota.

Attaching and detaching run as tasks: the VM's definition changes first,
then its host plugs the disk in or out. Should that fail, or its answer be
lost, the task asks the VM what it has and settles on that, so the disk is
never attached in the records but not in the VM, or the other way round.
Meanwhile the VM is not started, stopped, migrated, snapshotted or backed
up.

VMs plug disks in while they run: data disks sit on a virtio-SCSI
controller, at places of their own. Not while a VM has snapshots: they
cover the disks it had.

Deleting a VM deletes the disks made with it (its boot disk, and the copies
a clone or restore made); disks of their own attached to it are kept,
attached to nothing, unless you tick "Its disks too" (`?delete_disks=true`).

A disk on local storage attaches to VMs on its host. Attached, it moves
with its VM when that migrates. Attached to nothing, it moves on its own
(Move): to another host, or into another pool (and back). Attaching it to
a VM on another host moves it there first. The host that has it serves it
read-only with `qemu-nbd` over TLS, with credentials made for that copy
alone, on a port of the migration range (49152-49215), and the other host
copies it in; a host that reaches both copies it alone (between pools),
straight into and out of Ceph. A copy goes to a name of its own and then
replaces what is there, so a copy left by a move that failed is never
taken for the disk. The copy carries what its image held: it depends on
nothing there. Once copied, the disk is recorded where it went and its
old copy is removed, or, should its host not answer, removed later (a
copy is never removed where its disk is). A move the controller was
interrupted in goes on from where the disk is. Disks on Ceph move through
a host that reaches both their pool and where they go. Two pools on the
same storage (the same NFS export, the same Ceph pool) are refused: a
disk moved between them would be its own copy. A host with disks on its
storage, or disks moving there, is not decommissioned.
