# Images

An image is added from a URL or from a file (Add image), into the image
store of the storage pool it is kept in (Keep it in, among the pools the
tenant may use):

- **Local disk of a host** (the default): a connected host picked at
  random keeps it, in `images/<tenant>/<sub-tenant>/…/<image ID>.qcow2`
  under the agent's data directory, the tenants named by their IDs.
- **A shared pool**: a host attached to it (picked at random) stores it
  there, once: the same path on an NFS pool, an RBD image
  `store_<tenant>_…_<image ID>` on Ceph.

Either way its file comes to that host first, in parts, and the host then
checks it and adds it to the store (Checking and storing). From a URL, the
controller downloads it as it is added, passing each part on as it comes:
hosts download nothing. That is the only time it is downloaded. The image
keeps no URL (the audit log records where it came from), and nothing
downloads it again later: a download
cut off (the controller went away) fails within ten minutes, for good.
From a file, the browser uploads it (below).

The images page shows how far it is, then its size, where it is kept and
which hosts hold a copy. Adding one that failed says why, and has failed
for good (as has one whose storing stopped without saying how, within
minutes): delete the image and add it again. Two images are added at a
time per tenant tree (a tenant of the provider's with all its sub-tenants;
the provider's own count apart), and a server that sends nothing for a
minute, sends less than 16 MiB in five minutes, or takes over two hours,
is given up on. Deleting an image stops what adds it. Two hashes are
recorded: the SHA-256 of the file as it came, and the image's digest, the
SHA-256 of the disk it holds (read as a raw disk).

## Uploading

From a file (Add image → From a file on this computer), the image is added
with its size, and the browser sends the file in parts of 16 MiB (the API
takes up to 32 MiB each: `PUT /api/v1/images/{id}/upload?offset=N`, the
next part from what has arrived); the controller passes each to the host,
as it does a download's. A stopped upload (the page closed, the connection
lost) is resumed from what arrived: Resume upload, and the same file
again. Once all of it is there it is stored as a download is, with the
same checks, limits and hashes. An upload no part reaches for a day fails,
and what arrived goes.

## Copies on hosts

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
decommissioned, say), VMs are not made from it until one is: it is not
downloaded again. Keep images that matter in a shared pool, or let several
hosts hold copies (each VM made on a host leaves one there); an image whose
copies are all gone is deleted and added again. An image in a store is
never replaced: hosts storing the same one in a shared pool at once keep
the first. Only a copy that does not hold the image's digest is replaced
(one left by storing cut off). Every hour, each host's store, and each
shared pool's, lets go of images that no longer exist, and of what stores,
copies and incoming files cut off left.

A tenant's image is downloaded from a public internet address only, and
may describe a disk of at most the host's largest image (`image_max_gib` in
`agent.toml`, 64 GiB by default); the provider's may come from anywhere
and describe disks up to 1 TiB. Both are checked before the file is
unpacked: a small file can unpack into a huge disk. Images count
against their tenant's image quota (Images, in GiB) at their stored size,
once, however many hosts hold them; none is added where it is used up.
