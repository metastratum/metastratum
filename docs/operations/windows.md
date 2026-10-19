# Windows images

`deploy/images/windows/build.sh` builds a Windows Server image from an
installation ISO and the virtio-win ISO, unattended, in a VM of the user's
own libvirt session (no root needed; 20 to 60 minutes):

```
deploy/images/windows/build.sh --iso SERVER_EVAL_x64FRE_en-us.iso \
  --virtio virtio-win-0.1.302.iso --out windows-server-2025.qcow2
```

By default it installs Datacenter with Desktop Experience (`--index 4` on
Windows Server 2022 and 2025 ISOs; 3 is Datacenter without the desktop,
much smaller), with the virtio drivers and QEMU's guest agent,
cloudbase-init (it downloads the current stable), Remote Desktop and ping
allowed, and the serial console (Special Administration Console) on. The
image is generalized with sysprep, so the build's own account and password
go with it. Serve the result where hosts can download it, and add it with
operating system Windows (only the provider's images may come from internal
addresses).

VMs made from a Windows image run with Hyper-V enlightenments. Their
password (Administrator's) is chosen as they are created, and has to meet
Windows' rules (8 characters or more, with three of upper case, lower case,
digits and symbols). Windows takes it as it is, not hashed, so it is kept
sealed with the master key, put in the VM's cloud-init seed for its first
boot only, and dropped once the guest reports that boot done, through its
guest agent; the VM's details say while it is pending. A VM made from a
Windows image needs one: until it is set, the Administrator has a random
password no one knows (sysprep would leave it blank). Clones and restored
VMs keep their source's unless given another. Windows VMs take no SSH keys.

Windows 11 needs Secure Boot and a TPM (see [Secure Boot and TPM](boot-security.md)).
