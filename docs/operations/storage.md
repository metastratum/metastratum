# Deleting pools and images

A storage pool that holds no VM disks and keeps no images can be removed
two ways:

- **Unlink** (the default): every host unmounts it and forgets its
  credentials; the storage is left as it is, cached images included.
  Adding the pool again later finds them there.
- **Delete**: also removes metastratum's data from the storage: its cached
  images, and volumes if any were left (on NFS, `images/` and `volumes/`
  files named after metastratum IDs; on Ceph, `img-*` images). Other files
  there stay. If another pool uses the same storage (the same NFS export,
  or the same Ceph pool), the data stays: that pool needs it.

Deleting an image removes every host's copy of it (see [Images](images.md)). Hosts that
are down at the time clean up when they reconnect. The outcome is in the
audit log (`storage_pool.cleanup`, `image.cleanup`).
