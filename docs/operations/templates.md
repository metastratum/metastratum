# Templates and clones

Clone a stopped VM from its page (Clone): the new VM gets copies of its
disks, its CPUs, memory and firmware, in the same tenant and storage, on a
new network interface. It boots as a new machine: cloud-init gives it its
own network, host name and SSH host keys, and the password or keys given
when cloning; the disks keep everything else. On local storage the clone
is made on the same host; on NFS and Ceph anywhere the cluster has room.
Copies are whole: they depend on nothing, as disks made from images do.

Make a stopped VM a **template** (Details → Template) to keep it for
cloning: a template is never started, and Create VM offers it next to the
images. Prepare it first: install what its clones need, then clear what
must differ between them (`sudo cloud-init clean --machine-id`), and power
it off.

While a clone is being made, its source is not started, deleted,
snapshotted or backed up.
