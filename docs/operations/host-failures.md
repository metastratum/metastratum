# When a host fails

With HA enabled on its cluster (Infrastructure → Clusters), the leader
watches the cluster's hosts. A host has failed when its agent has been
silent for 60 seconds **and** it does not answer on its SSH port either (a
crashed agent on a healthy host is not a failed host), and
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
