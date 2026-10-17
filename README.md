# metastratum

metastratum turns a group of Linux servers into a private cloud for virtual
machines, with a web UI and an API. It is built on the components the big
platforms use, KVM, libvirt and OVN, and aims to be easy to run: one or more
controllers with PostgreSQL behind them, and an agent on each host.

- **Virtual machines** from cloud images, configured with cloud-init, with a
  graphical and a serial console in the browser, limits on disk and
  network bandwidth, and more CPUs, memory or disk when they need it.
  Clone them, or keep templates to make new ones from.
- **Live migration** between hosts, on shared storage (NFS, Ceph RBD) or
  local disks. Putting a host in maintenance moves its VMs elsewhere.
- **Snapshots** to put a VM's disks back to, and **backups** of running
  and stopped VMs to NFS, taken by hand or on a schedule and restored as
  new VMs.
- **Networks**: private networks with routers, floating IPs and outbound
  NAT, and security groups, on OVN.
- **Tenants** for teams or customers, each with quotas, roles, optionally
  its own identity provider (OpenID Connect) and its own sign-in address.
  Tenants can have sub-tenants.
- **High availability**: controllers run as several replicas, and when a
  host fails, its VMs on shared storage restart on other hosts once it is
  fenced.
- **Security** built in: mutual TLS between controllers and hosts,
  short-lived host certificates with revocation, secrets sealed in the
  database.

metastratum is in development and has no release yet.

- [Getting started](docs/getting-started.md): packages, the first
  controller and the first host
- [Running metastratum](docs/operations.md): controllers in several places,
  PostgreSQL and OVN underneath, host failure, resizing, templates,
  snapshots and backups, upgrades, monitoring
- [Security](SECURITY.md): the security model, and what the last review
  found

## Layout

| Path | What |
|---|---|
| `crates/mst-core` | Shared domain types (IDs, tenant paths, VM specs) |
| `crates/mst-db` | Postgres repositories |
| `crates/mst-authz` | Permissions: built-in roles inherited down the tenant tree |
| `crates/mst-scheduler` | VM placement: filters and weighers over host capacity |
| `crates/mst-pki` | Internal CA, certificate issuance, join tokens |
| `crates/mst-proto` | gRPC protocol between controller and agents (`proto/agent.proto`) |
| `crates/mst-virtrpc` | Native async client for libvirt's RPC protocol, generated from `remote_protocol.x` |
| `crates/mst-libvirt` | Domain XML and the agent's libvirt facade (events, consoles) |
| `crates/mst-netplan` | The network plane as a pure function: model → OVN northbound rows → one transaction |
| `crates/mst-ovsdb` | OVSDB client for the OVN northbound database |
| `crates/mst-storage` | Host storage pools: local qcow2, NFS, Ceph RBD |
| `crates/mst-controller` | Control plane: REST API, agent endpoint, task engine, console relay |
| `crates/mst-agent` | Host agent: drives libvirt, storage and consoles on each hypervisor |
| `crates/mstctl` | Admin CLI (placeholder) |
| `web/` | Web UI (Vue 3 + TypeScript) |
| `tests/e2e` | End-to-end tests against a running controller and lab |
| `migrations/` | SQL migrations (sqlx) |
| `deploy/dev` | Local dev dependencies (Postgres, Redis, Dex as a test identity provider) |
| `deploy/systemd` | Units for the controller and the agent |
| `deploy/config` | Every setting of the controller and the agent, as examples |
| `deploy/packaging` | `.deb` and `.rpm` packages (`build.sh`) |
| `docs/` | Operating metastratum |
| `deploy/lab` | Nested-KVM lab: 1 control VM + 3 hypervisor VMs |

## Development

Building needs a Rust toolchain and `protoc`; nothing links libvirt's C
library. The agent ships as a static binary, which needs the musl target. The
tests need Postgres, which `deploy/dev/compose.yaml` provides in Docker:

```sh
sudo pacman -S --needed rustup protobuf docker   # Arch; Debian: protobuf-compiler
rustup target add x86_64-unknown-linux-musl
docker compose -f deploy/dev/compose.yaml up -d
export DATABASE_URL=postgres://metastratum:metastratum@127.0.0.1:55432/metastratum
cargo test
```

Check dependencies for known vulnerabilities with `cargo audit` (install
it with `cargo install cargo-audit`; exceptions, each with its reason, are in
`.cargo/audit.toml`) and `npm audit` in `web/`.

Some tests need extra tools or services and skip themselves without them:

- `virt-xml-validate` and `qemu-img` (ship with libvirt and QEMU)
- a libvirt daemon: `MST_TEST_LIBVIRT_SOCKET=/run/libvirt/virtqemud-sock`
  (your user needs `qemu:///system` access; the tests create and remove
  small throwaway domains)
- a Redis server, for shared rate limits: `MST_TEST_REDIS_URL=redis://127.0.0.1:56379`
  (in the dev compose file)
- empty OVN northbound databases that nothing else uses (plain, over TLS,
  and a three-server cluster): `deploy/lab/lab.sh testdb` starts them on the
  lab and prints the `MST_TEST_OVN_NB_*` variables to export

The web UI lives in `web/`:

```sh
cd web
npm install
npm run dev     # http://localhost:5173, proxies /api to a controller on 127.0.0.1:18080
npm test        # unit tests
npm run api     # regenerate the typed API client after changing the REST API
```

## Lab

`deploy/lab/lab.sh` builds a nested-KVM lab on `qemu:///system`:

| VM | Address | Role |
|---|---|---|
| `mst-ctl` | 10.77.0.10 | Postgres, OVN central (NB/SB, northd), NFS exports (storage, backups), single-node Ceph |
| `mst-host1..3` | 10.77.0.11–13 | Hypervisors: libvirt/QEMU, OVS, ovn-controller |

It needs about 25 GiB of RAM, 14 vCPUs and roughly 15 GB of disk in
`/var/lib/libvirt/images/mst-lab` (plus a 30 GB thin disk for Ceph).

### One-time host setup

1. **Packages** (Arch names):

   ```sh
   sudo pacman -S --needed libvirt qemu-system-x86 edk2-ovmf dnsmasq virt-install cdrtools curl openssh
   ```

   The lab VMs boot with UEFI, so `edk2-ovmf` is required. The Debian cloud
   image loops in GRUB under legacy BIOS (SeaBIOS) on recent KVM/QEMU.

2. **libvirt daemons.** Enable the modular daemons' sockets:

   ```sh
   sudo systemctl enable --now virtqemud.socket virtnetworkd.socket virtstoraged.socket
   ```

3. **libvirt group.** Run `sudo usermod -aG libvirt $USER`, then log in again.
   The script runs as your user against `qemu:///system` and never uses sudo.

4. **Nested virtualisation.** The hypervisor VMs need their own `/dev/kvm`.
   Check with `cat /sys/module/kvm_{intel,amd}/parameters/nested 2>/dev/null`,
   which should print `1` or `Y`. To enable it, run one of these, then reload
   the module or reboot:

   ```sh
   echo 'options kvm_amd nested=1' | sudo tee /etc/modprobe.d/kvm-nested.conf
   echo 'options kvm_intel nested=1' | sudo tee /etc/modprobe.d/kvm-nested.conf
   ```

5. **Docker hosts only.** Docker sets the iptables `FORWARD` policy to `DROP`,
   which silently blocks NAT for the libvirt networks. The VMs get DHCP and DNS
   but cannot reach the internet, so package installs fail. Allow the libvirt
   bridges:

   ```sh
   sudo iptables -I DOCKER-USER -i virbr+ -j ACCEPT
   sudo iptables -I DOCKER-USER -o virbr+ -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
   ```

   These rules are lost on reboot or when Docker restarts. For a permanent
   fix, add `{"ip-forward-no-drop": true}` to `/etc/docker/daemon.json`, then
   run `sudo systemctl restart docker`. Restarting Docker restarts its
   containers.

### Usage

```sh
deploy/lab/lab.sh up        # create networks, storage pool and VMs; wait until ready
deploy/lab/lab.sh status
deploy/lab/lab.sh ssh mst-host1 [cmd...]
deploy/lab/lab.sh ceph      # (re)run the Ceph setup; prints the RBD pool config
deploy/lab/lab.sh ovn-tls   # serve OVN's databases over TLS, with the controller's certificates (see the script)
deploy/lab/lab.sh testdb    # throwaway OVN northbound databases for tests
deploy/lab/lab.sh down      # remove VMs and networks; keeps the cached base image
```

`up` also sets up Ceph on `mst-ctl` (cephadm; one monitor, manager and OSD,
pool `metastratum`, client `client.metastratum`) and installs the `rbd` tools on
the hypervisors. On a lab created before Ceph was added, `lab.sh ceph` grows
`mst-ctl` and restarts it once.

`up` checks the requirements above first. Once the VMs boot, it verifies
internet access through NAT and that every hypervisor VM has a usable
`/dev/kvm`. If anything fails, it stops with the fix.

### Running metastratum against the lab

The controller runs on your machine and is reachable by the lab hosts as
`10.77.0.1`, the host side of the lab network. It uses the local dev Postgres
and the lab's OVN.

`deploy/dev/controller.toml` configures the controller for that; every
command reads it through `MST_CONFIG`:

```sh
cargo build -p mst-controller
(cd web && npm run build)
export MST_CONFIG=deploy/dev/controller.toml
# Secrets in the database (the CA key, storage keys, identity provider
# secrets) are sealed with a master key kept outside it, beside the file.
(umask 077; target/debug/mst-controller gen-master-key > deploy/dev/master.key)
MST_ADMIN_PASSWORD='choose-a-long-password' target/debug/mst-controller bootstrap
target/debug/mst-controller serve
```

Whoever reaches OVN's databases can rewrite every tenant's network and every
host's flows, so beyond trying things out, use TLS. Write the certificates
for `mst-ctl`, install them, and switch `ovn` in the configuration to
`northbound = ["ssl:10.77.0.10:6641"]` and
`southbound = ["ssl:10.77.0.10:6642"]`; after a restart the agents switch
their hosts over:

```sh
target/debug/mst-controller ovn-certificate --name mst-ctl --address 10.77.0.10 \
  --out-dir deploy/lab/.state/ovn-central
deploy/lab/lab.sh ovn-tls
```

Then build the agent's package and install it on the three hosts; hosts that
are not enrolled yet get a fresh join token. Building packages takes
[nfpm](https://nfpm.goreleaser.com) (`deploy/packaging/install-nfpm.sh`
installs it):

```sh
deploy/lab/agent.sh   # with MST_CONFIG set, as above
```

Live migrations are driven by the controller, step by step on each host's
own libvirt; the hosts' libvirt daemons never talk to each other, and do not
listen on the network. QEMU sends the VM from host to host over TLS, with
the host certificate (signed by the controller's CA) that the agent
installs where QEMU looks for it (`/etc/pki/qemu`). Hosts without it show
"No migration" and are not chosen as destinations. VMs get their cluster's
baseline CPU model, so they run on, and migrate between, any of its hosts.
A VM can instead be created non-migratable: it then gets its host's full CPU
and disks use the host's page cache, and it stays on its host (evacuation
leaves it there). That is the default when the VM's cluster has a single
host. The setting can be changed while the VM is stopped and applies from
its next start.

Set `metrics_listen = "127.0.0.1:9180"` for Prometheus metrics. To try out
several controllers, run a second one beside it from a copy of the file
with other `api.listen` and `agents.listen` ports (the same database and
master key), and give the agents both endpoints:
`MST_CONTROLLER=https://10.77.0.1:8443,https://10.77.0.1:8444 deploy/lab/agent.sh`.

Open http://127.0.0.1:18080 and sign in as `admin`. Add an image (for example
`https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2`)
before creating VMs. Hosts only take VMs once they are in a cluster
(Infrastructure → Clusters, then Manage on a host); sub-tenants also need that
cluster granted to them (Tenants → Resources).

Tenants can sign in through their own OpenID Connect provider (Tenants →
Sign-in). For trying that out, the dev compose file runs
[Dex](https://dexidp.io/) at `http://127.0.0.1:5556/dex`, with client ID
`metastratum`, secret `metastratum-dev-secret`, and a mock login that signs everyone
in as Kilgore Trout in group `authors` (add `groups` to the scopes). The
controller builds its redirect URI from the address the browser reached it
at, so both `http://127.0.0.1:18080` and the Vite dev server
(`http://127.0.0.1:5173`) work; Dex lists both.

A tenant can also get its own address (Tenants → Sign-in → Domains), whose
sign-in page starts from that tenant. Browsers send `*.localhost` names to
your own machine, so binding `acme.localhost` to a tenant and opening
`http://acme.localhost:18080` tries it out.

### End-to-end tests

Both suites run against the controller above and skip unless pointed at it:

```sh
MST_E2E_URL=http://127.0.0.1:18080 MST_E2E_PASSWORD=... cargo test -p mst-e2e -- --nocapture
cd web && MST_E2E_URL=http://127.0.0.1:18080 MST_E2E_PASSWORD=... npm run e2e
```

The Rust suite creates two VMs, checks both consoles, pings across the overlay
network between hosts, power-cycles and deletes them; checks tenant isolation,
inherited roles, quotas, the audit log and API tokens; and boots VMs from the
lab's NFS export and, with `MST_E2E_RBD_KEY` set to the key `lab.sh ceph`
prints, from Ceph RBD. For networking it builds VPCs in a child tenant and
checks isolation between them, SNAT to the lab's external network
(10.78.0.0/24, registered as `lab-external` on first run), a floating IP that
only answers SSH while a security group allows it, and a parent's guardrail
that closes it again regardless. For live migration it moves VMs on local
disk, NFS and Ceph while another VM pings them and holds a TCP connection to
them, and checks the guests never restarted; with `MST_E2E_EVACUATE=1` it also
evacuates a host (which moves every VM on it, not only the test's). With `MST_E2E_DEX=http://127.0.0.1:5556/dex` it also
signs in through Dex and checks how groups become roles. It also checks that
live updates (`/api/v1/events`) reach only those allowed to see the change.
For hardening it checks that tenants' images (and, with Dex, identity
providers) cannot reach internal addresses, and that hosts renew their
certificates and migrate with the new ones. With a second replica in
`MST_E2E_URL2` it checks both serve every host; with `MST_E2E_FENCE=1` it
freezes a lab host, waits for it to be fenced and its VM restarted on
another, then starts the host again and returns it to service. Cluster
limits are checked from inside guests: a disk write and a transfer between
two VMs run at the limits' pace, and speed up once they are lifted. Backups
are taken of a running and a stopped VM into the lab's backup export, by hand
and on a schedule, and restored as new VMs that find a file written before
the backup; they outlive their VM. Snapshots are taken of a running and a
stopped VM on local disk, NFS and Ceph, reverted to, and deleted; on NFS the
VM migrates with them. Resizing grows a running VM's disk (the guest sees
it), and a stopped VM's CPUs, memory and disk (its file system grows at
boot). A stopped VM made a template is never started, and its clones on
local disk, NFS and Ceph boot with what its disk held under their own name.
`MST_E2E_DEBUG=1` prints
console traffic. The browser suite does the VM lifecycle and tenant
management through the UI with Playwright, using your installed Chrome.

## License

MIT; see [LICENSE](LICENSE). The exception is libvirt's protocol definition in
`crates/mst-virtrpc/proto/`, which keeps its LGPL-2.1-or-later licence and ends
up in `mst-agent`. [LICENSING.md](LICENSING.md) explains how the two fit
together, what distributing agent binaries involves, and how to rebuild the
agent with a modified protocol definition.
