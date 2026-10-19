# Images

Adding an image downloads it at once into the image store of the storage
pool it is kept in (Keep it in, among the pools the tenant may use):

- **Local disk of a host** (the default): a connected host picked at
  random keeps it, in `images/<tenant>/<sub-tenant>/…/<image ID>.qcow2`
  under the agent's data directory, the tenants named by their IDs.
- **A shared pool**: a host attached to it downloads it there, once: the
  same path on an NFS pool, an RBD image `store_<tenant>_…_<image ID>` on
  Ceph.

The images page shows when it is there, its size, where it is kept and
which hosts hold a copy; a download that failed says why, and can be tried
again (Download again), as can one that stopped without saying how. Two
images download at a time per tenant tree (a tenant of the provider's with
all its sub-tenants; the provider's own count apart), and a server that
sends nothing for a minute, sends less than 16 MiB in five minutes, or
takes over two hours, is given up on. Deleting an image stops its download.
Two hashes are recorded: the SHA-256 of the file downloaded, and the
image's digest, the SHA-256 of the disk it holds (read as a raw disk).

A VM made from an image is made on its host from that host's copy, or
straight from the shared pool keeping it when the host is attached to that
pool. A host without either gets a copy from a host that has one (a host
attached to the shared pool first copies it into its own store): served
read-only with
`qemu-nbd` over TLS, with credentials for that copy alone, on a port of the
migration range (49152-49215), as disks move. The host copying it checks
the copy holds the image's digest, and throws it away otherwise: a host
handing over something else (a compromised one) spreads nothing. Hosts
keep their copy, and each is a source for the next. The VM's disk is a copy of the image of its
own, in its pool: it depends on nothing, so a VM migrates without its
image, and keeps working once the image is deleted. VMs made while the
image downloads wait for it (deleting such a VM ends the wait). Each
controller copies up to four images onto hosts at once; VMs waiting for a
copy keep no other task from running.

Should no copy of an image be reachable (its hosts are down or
decommissioned, say), the host that needs it downloads it again, from its
address; what it downloads must then hold the same disk (the same digest),
or the VM is not made. An image in a store is never replaced: hosts
storing the same one in a shared pool at once keep the first. Only a copy
that does not hold the image's digest is replaced (one left by a download
cut off, before the image's address served another file). Every hour,
each host's store, and each shared pool's, lets go of images that no longer
exist, and of what downloads and copies cut off left.

A tenant's image comes from a public internet address only, and may
describe a disk of at most the host's largest download (`image_max_gib` in
`agent.toml`, 64 GiB by default); the provider's may come from anywhere
and describe disks up to 1 TiB. Both are checked before the download is
unpacked: a small download can unpack into a huge disk. Images count
against their tenant's image quota (Images, in GiB) at their stored size,
once, however many hosts hold them; none is added where it is used up.
