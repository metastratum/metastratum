# Resizing

Resize a VM from its page (Resize). CPUs and memory beyond what they may
grow to while it runs (below) change while it is stopped; it gets them at
its next start, when its host needs room for them (see below). Disks only
grow, running or
not: a running guest sees the larger disk at once, and cloud images grow
their partition and root file system at the next boot (cloud-init's
growpart); otherwise grow them in the guest (`growpart`, `resize2fs`).
qcow2 disks (local and NFS storage) cannot grow while they hold
snapshots, so delete the VM's snapshots first; Ceph disks can. What grows
counts against the tenant's quotas.

## CPUs and memory while a VM runs

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

The host keeps the maximums for the VM while it runs, and they count
against the tenant's `vcpus` and `memory_mib` quotas, so growing within
them never fails for want of room. Raising a maximum itself needs the VM
stopped.

## Room on hosts

A host keeps CPUs and memory (each VM's maximums, within the cluster's
overcommit) for the VMs meant to run on it; stopped VMs keep nothing there,
and are not counted in the memory a host shows as allocated. So starting a
stopped VM needs room on its host again. If its host has none, a VM whose
disks are all on shared storage starts on another host of its cluster that
has (the scheduler's pick, with every feature of its CPU), and its old host
lets go of it; one with disks on its host's own storage starts only there,
and says what is lacking. Tenants' quotas count every VM, stopped or not.

Guests also hand memory they free back to their host as they free it
(free page reporting), so a host's actual use stays below what it keeps
for its VMs. It can be turned off per VM (Details → Free memory, or
`free_page_reporting` in the API and Terraform), for a guest that should
keep its memory: one that frees and reuses a lot of it quickly pays for
getting it back from the host each time. It is part of the VM's balloon,
so it changes while the VM is stopped and applies from its next start.
