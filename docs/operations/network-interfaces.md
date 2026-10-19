# Network interfaces

A VM gets one interface as it is made. More can be added later, each in a
network of its tenant's (`POST /vms/{id}/nics`, or Add interface on the
VM), up to eight; one can move to another network (`PATCH
/vms/{id}/nics/{nic}`), keeping its MAC address and security groups and
getting an address there; and one can go (`DELETE`), as long as the VM
keeps one. A removed interface's floating IP is kept, attached to nothing,
and it leaves its load balancers. An interface with a floating IP, or in
load balancers, moves only within networks their VPC reaches (detach it
first otherwise).

A running VM keeps running throughout: a new interface is plugged in at
once (four of them; VMs started before this release, one until they next
start), and one that moves is unplugged and plugged in again, so the guest
sets it up afresh and asks DHCP for its new address (settings made by hand
in the guest for that interface go). Guests configure new interfaces with
DHCP: Windows does so by itself, and Linux VMs' cloud-init seed configures
every virtio interface so, the first one's default route preferred (VMs
made before this release configure only their first one). A VM with
interfaces plugged in while it ran migrates as it is: the destination lays
its devices out as the source has them.
