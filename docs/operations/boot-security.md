# Secure Boot and TPM

Any VM can boot with UEFI Secure Boot (OVMF with Microsoft's keys enrolled:
Windows, and Linux distributions' shim, boot) and have a TPM 2.0 (swtpm).
An image says whether VMs made from it get them by default; each VM can be
made with or without, and changed while it is stopped. A VM switched to
or from Secure Boot starts with its UEFI settings made anew (boot entries
too: Windows and Linux images boot from the fallback path).

A TPM's state is the VM's: it lives on the host the VM runs on and goes
where the VM goes. A live migration carries it; a stopped VM started on
another host (its own had no room) brings it from the host it left. The
leader keeps a copy in the database, sealed with the master key, at most a
minute old, for a VM restarted elsewhere after its host failed (whatever
the guest wrote to its TPM in that last minute is lost). A TPM taken away
from a VM is gone, and one given again is new; clones and restored VMs get
new ones (a TPM's keys are its own VM's).

Hosts need `swtpm` and `swtpm-tools`, and OVMF firmware with Secure Boot
(`ovmf` on Debian and Ubuntu, `edk2-ovmf` elsewhere); the installer puts
them on. A host without them shows "No TPM" or "No Secure Boot", and VMs
that need them are not placed there.
