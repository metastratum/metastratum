# Windows images

`deploy/images/windows/build.sh` builds a Windows Server or Windows 11
image from an installation ISO and the virtio-win ISO, unattended, in a VM
of the user's own libvirt session (no root needed; 20 to 60 minutes):

```
deploy/images/windows/build.sh --iso SERVER_EVAL_x64FRE_en-us.iso \
  --virtio virtio-win-0.1.302.iso --out windows-server-2025.qcow2
deploy/images/windows/build.sh --windows-11 --iso Win11_25H2_English_x64.iso \
  --virtio virtio-win-0.1.302.iso --out windows-11.qcow2
```

For Windows Server it installs Datacenter with Desktop Experience by
default (`--index 4` on Windows Server 2022 and 2025 ISOs; 3 is Datacenter
without the desktop, much smaller); for Windows 11, Pro (`--index 6` on
Microsoft's multi-edition ISOs), on a 64 GiB disk, as Windows 11 needs; its
build VM has a TPM and Secure Boot too, so the build machine needs `swtpm`.
Images get the virtio drivers and QEMU's guest agent, cloudbase-init (it
downloads the current stable), Remote Desktop and ping allowed, and on
Windows Server the serial console (Special Administration Console). The
image is generalized with sysprep, so the build's own account and password
go with it. Windows 11 does not encrypt its disk by itself: a VM's disk
does not depend on its TPM. Its Smart App Control (which, after watching
what a new installation runs, may switch itself on and from then on block
software without a good reputation with Microsoft) is off, so VMs do not
change how they run software on their own; off, it cannot be switched on
again without reinstalling Windows. `--keep-smart-app-control` leaves it
as Windows has it. Serve the result where the controller can download it
(or upload it),
and add it with operating system Windows (only the provider's images may
come from internal addresses): Windows Server's with default account
`Administrator`, Windows 11's with Secure Boot and TPM and no default
account (its VMs each name theirs; its own Administrator is off).

VMs made from a Windows image run with Hyper-V enlightenments. Each is
signed in to with an account named as it is created (`username`): by
default the image's (its default account, `Administrator` for Windows
Server), required for images without one. The account is made as the VM
first boots if it is not there, in Administrators. Its password is chosen
as the VM is created, and has to meet Windows' rules (8 characters or more,
with three of upper case, lower case, digits and symbols, and not the
account's name); it does not expire. Windows takes it as it is, not
hashed, so it is kept sealed with the master key, put in the VM's
cloud-init seed for its first boot only, and dropped once the guest reports
that boot done, through its guest agent; the VM's details say while it is
pending. A VM made from a Windows image needs one: until it is set, the
image's accounts have random passwords no one knows (sysprep would leave
them blank). A VM made with user data of its own needs none: the user
data is used as it is given, and `password` and `username` go unused. Clones and restored VMs keep their source's account, and its
password unless given another. Windows VMs take no SSH keys.

A Windows VM's computer name is its name cut to 15 characters (Windows'
NetBIOS limit): give Windows VMs names that differ in their first 15.

Windows 11 needs Secure Boot and a TPM (see [Secure Boot and TPM](boot-security.md)), and 4 GiB of memory.
It sleeps when idle, as desktops do: starting it wakes it, and VMs that
should stay reachable are made with sleep off (see [Sleep](sleep.md)).
