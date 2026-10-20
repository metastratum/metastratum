# Sleep

A guest may sleep (suspend to RAM, or to disk) as on hardware: desktops,
Windows 11's among them, do when idle. Asleep, a VM keeps its memory on its
host and shows as Asleep (power `suspended`); starting it (Wake in the UI,
`POST /vms/{id}/start`) wakes it.

Whether a VM may sleep is its own setting (`sleep`): as its image says by
default (images allow it unless added with `sleep` off), and changeable
while it is stopped, from its next start. Turned off, the guest is offered
neither kind of sleep: for VMs that should stay reachable, which nothing
wakes otherwise. Clones and restored VMs keep their source's setting.

A VM paused rather than asleep (by libvirt, or on a disk error) shows as
Paused, and starting it resumes it.
