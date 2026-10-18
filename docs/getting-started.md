# Getting started

The smallest metastratum: one controller and one hypervisor host. Adding
controllers, more hosts, high availability and the rest is in
[Running metastratum](operations.md).

You need:

- **A controller machine**: Linux with systemd. It runs the controller,
  PostgreSQL and OVN's central databases (they can move elsewhere later).
- **A host** for the VMs: Debian 13, Ubuntu 24.04 or Fedora (or newer), on
  a machine with hardware virtualisation (VT-x or AMD-V) switched on.
- A network between them that guests and tenants cannot reach. The
  examples use `10.0.0.10` for the controller and `10.0.0.11` for the host.

## The quick way: the installers

Two scripts set everything up from scratch, on Debian 13, Fedora 43 or
later, RHEL, Rocky Linux or AlmaLinux 10, or Arch Linux:

- [`deploy/install-controller.sh`](../deploy/install-controller.sh)
  installs PostgreSQL, OVN's central databases over TLS, the controller with
  its first administrator, and Caddy as the web UI's HTTPS proxy. VMs get
  the machine's own name servers by DHCP (`--dns` chooses others).
- [`deploy/install-agent.sh`](../deploy/install-agent.sh) makes a machine a
  host: libvirt and QEMU, Open vSwitch and OVN's host part, and the agent,
  enrolled with the controller. It also gives the VMs a way out: an external
  network (`10.250.250.0/24` by default) that the host NATs to its own
  network.

For everything on one machine, run both there; the agent enrolls with the
controller beside it by itself:

```sh
curl -fsSLO https://raw.githubusercontent.com/metastratum/metastratum/main/deploy/install-controller.sh
curl -fsSLO https://raw.githubusercontent.com/metastratum/metastratum/main/deploy/install-agent.sh
sudo bash install-controller.sh
sudo bash install-agent.sh
```

For more hosts, run `install-agent.sh` on each with a join token from the
controller (`mst-controller join-token --cluster default`, or Infrastructure
→ Hosts → Add host):

```sh
sudo MST_JOIN_TOKEN=mstjoin.... bash install-agent.sh --controller https://10.0.0.10:7821
```

`--help` shows the options of each (the web UI's domain name for a Let's
Encrypt certificate, another external network, a particular release...).
Both say what they will change and ask first, and can run again safely.

They use what the machine runs already rather than replacing it. A
PostgreSQL server gets a database and a user of its own, and a
`pg_hba.conf` line for them if it needs one (it is reloaded, never
restarted); `MST_DATABASE_URL` uses a database elsewhere instead. libvirt
stays as it runs, as `libvirtd` or as its per-driver daemons. Docker's
firewall drops forwarded traffic that is not its own: whenever Docker
starts, `metastratum-docker-forward.service` lets the external network's
bridge through (Docker's `DOCKER-USER` chain).
`--ovn-northbound` uses OVN's central databases elsewhere, and writes the
certificates their servers need. Before changing anything, each looks for
what it would clash with, reports all of it, and stops:

- ports in use by other programs (7820, 7821, 6641, 6642, and 80 and 443
  for the HTTPS proxy: `--https-port` serves the web UI on another port,
  and `--no-proxy` leaves the proxy to you);
- a database named `metastratum` left from an earlier installation whose
  `controller.toml` and master key are gone (the database's secrets are
  sealed with that key);
- OVN databases another deployment uses, a host's Open vSwitch in another
  OVN deployment, a bridge `br-ext` or physical network `external` in use,
  and libvirt TLS certificates the agent would replace. `--force` goes
  ahead with these. On
Arch, OVN is not packaged: the installers build it from source, and upgrade
the system first as Arch requires. On RHEL, Open vSwitch and OVN come from
the Fast Datapath repository, which takes a subscription; Rocky Linux and
AlmaLinux get them from the CentOS NFV SIG.

The rest of this page does the same by hand.

## Packages

Download `metastratum-controller` and `metastratum-agent`, as `.deb` or
`.rpm`, from the [releases](https://github.com/metastratum/metastratum/releases),
or build them from a checkout with `deploy/packaging/build.sh`. Both are
static binaries, so the same package works on any recent distribution.

## The controller

Install it with PostgreSQL and OVN's central databases (Debian and Ubuntu;
on Fedora: `dnf install ./metastratum-controller-*.rpm postgresql-server postgresql-contrib ovn-central`):

```sh
apt install ./metastratum-controller_*.deb postgresql ovn-central
```

Create the database. `createuser` asks for the password. (On Fedora, first
`postgresql-setup --initdb`, allow passwords for local connections in
`pg_hba.conf` (`scram-sha-256` instead of `ident`) and
`systemctl enable --now postgresql`.)

```sh
sudo -u postgres createuser --pwprompt metastratum
sudo -u postgres createdb --owner metastratum metastratum
```

Write the configuration and the master key, which seals the secrets kept in
the database. Both stay readable by root only; the service gets them as
systemd credentials.

```sh
install -m 600 /usr/share/doc/metastratum-controller/examples/controller.toml /etc/metastratum/controller.toml
(umask 077; mst-controller gen-master-key > /etc/metastratum/master.key)
```

In `/etc/metastratum/controller.toml`, set the database (with the password
you chose) and OVN's databases on this machine:

```toml
database_url = "postgres://metastratum:PASSWORD@localhost/metastratum"

[ovn]
northbound = ["ssl:10.0.0.10:6641"]
southbound = ["ssl:10.0.0.10:6642"]
```

Everything else has a default; the example file explains each setting.
Then switch OVN's databases to TLS with certificates from the controller,
create the first administrator, and start the controller:

```sh
mst-controller ovn-certificate --name ctl1 --address 10.0.0.10 --out-dir /root/ovn-ctl1
sh /root/ovn-ctl1/setup.sh
read -rs -p 'Administrator password: ' MST_ADMIN_PASSWORD && export MST_ADMIN_PASSWORD
mst-controller bootstrap && unset MST_ADMIN_PASSWORD
systemctl enable --now mst-controller
```

The web UI and API listen on `127.0.0.1:7820`, for an HTTPS proxy in front
([Web traffic](operations.md#controllers) says what it needs). To look
around before setting one up, forward the port over SSH and open
http://localhost:7820:

```sh
ssh -L 7820:127.0.0.1:7820 root@10.0.0.10
```

Sign in as `admin` with the password you chose.

## The host

Install the agent; it brings libvirt, QEMU and Open vSwitch with OVN's host
part (`dnf install ./metastratum-agent-*.rpm` on Fedora):

```sh
apt install ./metastratum-agent_*.deb
```

Tell OVN who this host is and which address carries the tunnels between
hosts. The agent connects it to the southbound database itself, with a
certificate of its own.

```sh
ovs-vsctl set open . external_ids:system-id=host1 \
  external_ids:ovn-encap-type=geneve external_ids:ovn-encap-ip=10.0.0.11
```

For floating IPs and outbound traffic, give OVN a bridge on the network
that leads outside (here the second NIC), named after the physical network
you will declare in metastratum (Infrastructure → External networks):

```sh
ovs-vsctl add-br br-ext -- add-port br-ext eth1
ovs-vsctl set open . external_ids:ovn-bridge-mappings=external:br-ext
```

Then enroll the host. In the web UI, choose Infrastructure → Hosts → Add
host, pick the `default` cluster and create a join token; it shows the
configuration to write. Or on the controller:

```sh
mst-controller join-token --cluster default
```

Put it in `/etc/metastratum/agent.toml` on the host, and start the agent:

```toml
controllers = ["https://10.0.0.10:7821"]
join_token = "mstjoin...."
```

```sh
chmod 600 /etc/metastratum/agent.toml
systemctl enable --now mst-agent
```

The host appears under Infrastructure → Hosts as soon as it connects. The
token works once; the agent keeps its own identity from then on.

## A first VM

1. Infrastructure → Storage: add a storage pool (a local directory on the
   hosts is enough to start).
2. Images: add a cloud image, for example
   `https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2`.
3. Machines → Create VM, with an SSH key or a password for the console. It
   goes on a default private network; to let it reach outside, attach that
   network's VPC to an external network (Networks).

## Check it

```sh
mst-controller doctor
```

compares the installation with the deployment checklist in
[SECURITY.md](../SECURITY.md#deployment-checklist) and says what to fix: the
HTTPS proxy, per-VM limits, fencing and so on. Ports to open: 7821 on the
controller for hosts, 6642 on the OVN node for hosts (6641 for controllers
only), UDP 6081 between hosts for OVN's tunnels, and 49152-49215 between
hosts for live migration.
