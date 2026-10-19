<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo/metastratum-mark-dark.svg">
  <img src="docs/logo/metastratum-mark.svg" width="64" height="64" alt="">
</picture>

# metastratum

metastratum turns a group of Linux servers into a private cloud for virtual
machines, with a web UI, an API and a Terraform provider. It is built on the
components the big platforms use, KVM, libvirt and OVN, and aims to be easy
to run: one or more controllers with PostgreSQL behind them, and an agent on
each host.

- **Virtual machines** from cloud images, set up with cloud-init, with
  graphical and serial consoles in the browser. Give them more CPUs, memory
  or disk, also while they run; limit their disk and network bandwidth;
  clone them, or keep templates to make new ones from.
- **Disks** of their own: attached to VMs (also while they run), detached,
  kept when their VM goes, and moved between hosts and storage pools.
- **Live migration** between hosts, on shared storage (NFS, Ceph RBD) or
  local disks. Putting a host in maintenance moves its VMs elsewhere.
- **Snapshots** to roll a VM's disks back to, and **backups** to NFS of
  running and stopped VMs: full or incremental (QEMU tracks the changed
  blocks), by hand or on a schedule, restored as new VMs.
- **Networks** on OVN: private networks with routers, outbound NAT and
  floating IPs, security groups, and load balancers.
- **Tenants** for teams or customers, each with quotas, roles, optionally
  its own identity provider (OpenID Connect) and its own sign-in address.
  Tenants can have sub-tenants.
- **High availability**: controllers run as several replicas, and when a
  host fails, its VMs on shared storage restart on other hosts once it is
  fenced.
- **Security** built in: two-factor sign-in (authenticator apps, security
  keys and YubiKeys), mutual TLS between controllers and hosts,
  short-lived host certificates with revocation, secrets sealed in the
  database.

## Getting started

On a machine with Debian 13, Ubuntu 26.04 or later, Fedora 43 or later,
RHEL, Rocky Linux or AlmaLinux 10, or Arch Linux, and hardware
virtualisation, two scripts set up a controller and make the machine its
first host:

```sh
curl -fsSLO https://raw.githubusercontent.com/metastratum/metastratum/main/deploy/install-controller.sh
curl -fsSLO https://raw.githubusercontent.com/metastratum/metastratum/main/deploy/install-agent.sh
sudo bash install-controller.sh
sudo bash install-agent.sh
```

They say what they will install and change, and ask first. The web UI is
then at the machine's address, over HTTPS. [Getting started](docs/getting-started.md)
covers more hosts, the scripts' options, and installing by hand.

## Documentation

- [Getting started](docs/getting-started.md): the installers, or packages
  by hand; the first controller, host and VM
- [Running metastratum](docs/operations/README.md): controllers in several
  places, PostgreSQL and OVN underneath, hosts and their failure, storage,
  networks, backups, two-factor sign-in, upgrades, monitoring
- [Security](SECURITY.md): the security model, a deployment checklist, and
  what the last review found
- [Terraform provider](terraform/README.md): VMs, networks and load
  balancers as code
- [Development](docs/development.md): the code, building it, and the
  nested-KVM lab it is developed on
- [Testing](docs/testing.md): unit, integration, end-to-end and browser
  tests

metastratum is young: expect things to change between
[releases](https://github.com/metastratum/metastratum/releases).

## License

MIT; see [LICENSE](LICENSE). The exception is libvirt's protocol definition in
`crates/mst-virtrpc/proto/`, which keeps its LGPL-2.1-or-later licence and ends
up in `mst-agent`. [LICENSING.md](LICENSING.md) explains how the two fit
together, what distributing agent binaries involves, and how to rebuild the
agent with a modified protocol definition.
