# Development

How the code is laid out, how to build it, and the lab it runs on: a
controller on your machine with three nested-KVM hypervisors. Tests, the
lab's end-to-end tests among them, are in [Testing](testing.md).

## Layout

| Path | What |
|---|---|
| `crates/mst-controller` | The control plane: REST API, agent endpoint, task engine, console relay |
| `crates/mst-agent` | The host agent: drives libvirt, storage and consoles on each hypervisor |
| `crates/mst-core` | Shared domain types (IDs, tenant paths, VM specs) |
| `crates/mst-db` | PostgreSQL repositories |
| `crates/mst-authz` | Permissions: built-in roles inherited down the tenant tree |
| `crates/mst-scheduler` | VM placement: filters and weighers over host capacity |
| `crates/mst-pki` | Internal CA, certificate issuance, join tokens |
| `crates/mst-proto` | The gRPC protocol between controller and agents (`proto/agent.proto`) |
| `crates/mst-virtrpc` | A native async client for libvirt's RPC protocol, generated from `remote_protocol.x` |
| `crates/mst-libvirt` | Domain XML, and the agent's libvirt facade (events, consoles) |
| `crates/mst-netplan` | The network plane as a pure function: model → OVN northbound rows → one transaction |
| `crates/mst-ovsdb` | OVSDB client for the OVN northbound database |
| `crates/mst-storage` | Host storage pools (local qcow2, NFS, Ceph RBD) and backups' files |
| `crates/mstctl` | Admin CLI (a placeholder) |
| `web/` | The web UI (Vue 3, TypeScript) |
| `terraform/` | The Terraform provider (Go), with an API client generated from the OpenAPI document |
| `tests/e2e` | End-to-end tests against a running controller and the lab |
| `migrations/` | SQL migrations (sqlx) |
| `docs/` | Running metastratum, and this |
| `deploy/install-*.sh` | The installers |
| `deploy/config` | Every setting of the controller and the agent, as examples |
| `deploy/systemd` | Units for the controller and the agent |
| `deploy/packaging` | `.deb` and `.rpm` packages (`build.sh`) |
| `deploy/dev` | Local dependencies (PostgreSQL, Redis, Dex as a test identity provider), and the controller's configuration for the lab |
| `deploy/lab` | The nested-KVM lab |

## Building

Building takes a Rust toolchain and `protoc`; nothing links libvirt's C
library. The agent ships as a static binary, built for the musl target:

```sh
sudo pacman -S --needed rustup protobuf docker   # Arch; Debian: protobuf-compiler
rustup target add x86_64-unknown-linux-musl
cargo build
```

The web UI:

```sh
cd web
npm install
npm run dev     # http://localhost:5173, proxying /api to a controller on 127.0.0.1:18080
npm run build   # into web/dist, which the controller serves
npm run api     # regenerate the typed API client after changing the REST API
```

### The demo

`npm run build:demo` builds a clickable demo into `web/dist-demo`: the UI
with a backend of its own running in the page (`web/src/demo`), seeded
with a provider, customer tenants, hosts, networks and VMs. It is static
files that work from any path on any web server (routes live in the URL's
fragment), so it can be uploaded anywhere; CI keeps each build as the
`demo` artifact. `npm run dev:demo` serves it while working on it, and
`npm run test:demo` (after `build:demo`) clicks through it in a browser,
as CI does.

Visitors start signed in as `admin` (the provider's administrator); signed
out, they can sign in as `alice` (tenant `northwind`) or `gina` (tenant
`globex`) too, each with the password `demo`. What they
change is kept in their browser until they press Reset. Tasks run on
timers, the serial console is a small scripted shell, and the graphical
console a drawn screen.

The demo's handlers are typed against the API's schema
(`src/api/schema.d.ts`): after the API changes, `npm run api` and the
typecheck point at the handlers to follow. They mirror the controller's
(`crates/mst-controller/src/api`), one file per area in
`web/src/demo/handlers`.

CI fails when the API clients (the web UI's and the Terraform provider's)
are not regenerated after the API changed. `deploy/packaging/build.sh`
builds the `.deb` and `.rpm` packages; it takes
[nfpm](https://nfpm.goreleaser.com), which
`deploy/packaging/install-nfpm.sh` installs.

Check dependencies for known vulnerabilities with `cargo audit` (`cargo
install cargo-audit`; exceptions, each with its reason, are in
`.cargo/audit.toml`) and `npm audit` in `web/`.

## The lab

`deploy/lab/lab.sh` builds a nested-KVM lab on `qemu:///system`:

| VM | Address | Role |
|---|---|---|
| `mst-ctl` | 10.77.0.10 | PostgreSQL, OVN central (NB/SB, northd), NFS exports (storage, backups), single-node Ceph |
| `mst-host1..3` | 10.77.0.11–13 | Hypervisors: libvirt/QEMU, Open vSwitch, ovn-controller |

It needs about 25 GiB of RAM, 14 vCPUs and some 15 GB of disk in
`/var/lib/libvirt/images/mst-lab` (and a 30 GB thin disk for Ceph). A
second network, 10.78.0.0/24, is the hosts' "external" network, for
floating IPs and outbound NAT.

### Setting up your machine, once

1. **Packages** (Arch's names):

   ```sh
   sudo pacman -S --needed libvirt qemu-system-x86 edk2-ovmf dnsmasq virt-install cdrtools curl openssh
   ```

   The lab's VMs boot with UEFI, hence `edk2-ovmf`: the Debian cloud image
   loops in GRUB under legacy BIOS on recent QEMU.

2. **libvirt's daemons:**

   ```sh
   sudo systemctl enable --now virtqemud.socket virtnetworkd.socket virtstoraged.socket
   ```

3. **The `libvirt` group:** `sudo usermod -aG libvirt $USER`, then log in
   again. The lab runs as you, against `qemu:///system`, without sudo.

4. **Nested virtualisation**, for the hypervisor VMs' own `/dev/kvm`.
   `cat /sys/module/kvm_{intel,amd}/parameters/nested 2>/dev/null` prints
   `1` or `Y` when it is on. Otherwise switch it on, then reload the module
   or reboot:

   ```sh
   echo 'options kvm_amd nested=1' | sudo tee /etc/modprobe.d/kvm-nested.conf    # AMD
   echo 'options kvm_intel nested=1' | sudo tee /etc/modprobe.d/kvm-nested.conf  # Intel
   ```

5. **With Docker:** its firewall (the iptables `FORWARD` policy `DROP`)
   blocks NAT for libvirt's networks. The VMs get addresses but no internet,
   so their packages fail to install. Let libvirt's bridges through:

   ```sh
   sudo iptables -I DOCKER-USER -i virbr+ -j ACCEPT
   sudo iptables -I DOCKER-USER -o virbr+ -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
   ```

   These rules go when Docker restarts or the machine reboots. To keep
   Docker from dropping forwarded traffic at all, add
   `{"ip-forward-no-drop": true}` to `/etc/docker/daemon.json` and restart
   Docker (which restarts its containers).

### Using it

```sh
deploy/lab/lab.sh up        # networks, storage pool and VMs; waits until they are ready
deploy/lab/lab.sh status
deploy/lab/lab.sh ssh mst-host1 [cmd...]
deploy/lab/lab.sh ceph      # (re)run the Ceph setup; prints the RBD pool's configuration
deploy/lab/lab.sh ovn-tls   # serve OVN's databases over TLS (see below)
deploy/lab/lab.sh testdb    # throwaway OVN northbound databases for the integration tests
deploy/lab/lab.sh down      # remove the VMs and networks; keeps the cached base image
```

`up` checks the requirements above first. Once the VMs boot, it checks they
reach the internet and that every hypervisor has a usable `/dev/kvm`, and
stops with the fix when not. It also sets up Ceph on `mst-ctl` (cephadm:
one monitor, manager and OSD; pool `metastratum`, client
`client.metastratum`) and the `rbd` tools on the hypervisors.

## Running metastratum against the lab

The controller runs on your machine, which the lab's hosts reach as
`10.77.0.1`. It uses the dev compose file's PostgreSQL and the lab's OVN,
as `deploy/dev/controller.toml` sets up; every command reads that file
through `MST_CONFIG`:

```sh
docker compose -f deploy/dev/compose.yaml up -d
cargo build -p mst-controller
(cd web && npm run build)
export MST_CONFIG=deploy/dev/controller.toml
# The secrets kept in the database are sealed with a master key kept beside the file.
(umask 077; target/debug/mst-controller gen-master-key > deploy/dev/master.key)
MST_ADMIN_PASSWORD='choose-a-long-password' target/debug/mst-controller bootstrap
target/debug/mst-controller serve
```

Open http://127.0.0.1:18080 and sign in as `admin`.

**OVN over TLS.** Whoever reaches OVN's databases can rewrite every
tenant's network and every host's flows, so beyond a first try, use TLS:
write the certificates for `mst-ctl` and install them there, then set
`northbound = ["ssl:10.77.0.10:6641"]` and
`southbound = ["ssl:10.77.0.10:6642"]` in the configuration's `[ovn]`.
After a restart, the agents move their hosts over.

```sh
target/debug/mst-controller ovn-certificate --name mst-ctl --address 10.77.0.10 \
  --out-dir deploy/lab/.state/ovn-central
deploy/lab/lab.sh ovn-tls
```

**The hosts.** `deploy/lab/agent.sh` builds the agent's package and
installs it on the three hosts, with a fresh join token (for the default
cluster) for those not yet enrolled (`MST_CONFIG` set as above; it takes
nfpm, see Building):

```sh
deploy/lab/agent.sh
```

**A first VM.** Add an image, for example
`https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2`.
Sub-tenants need the cluster granted to them (Tenants → Resources).

**Several controllers.** Run a second one beside the first, from a copy of
the configuration with other `[api] listen` and `[agents] listen` ports
(and the same database and master key), and give the agents both:

```sh
MST_CONTROLLER=https://10.77.0.1:8443,https://10.77.0.1:8444 deploy/lab/agent.sh
```

**Metrics.** Set `metrics_listen = "127.0.0.1:7822"` for Prometheus.

**Tenants' identity providers.** A tenant can sign in through its own
OpenID Connect provider (Tenants → Sign-in). The dev compose file runs
[Dex](https://dexidp.io/) for that at `http://127.0.0.1:5556/dex`: client
ID `metastratum`, secret `metastratum-dev-secret`, and a mock login that
signs everyone in as Kilgore Trout, in group `authors` (add `groups` to the
scopes). The controller makes its redirect URI from the address the
browser reached it at, so `http://127.0.0.1:18080` and the Vite dev server
(`http://127.0.0.1:5173`) both work; Dex knows both.

**Tenants' own addresses.** A tenant can have its own sign-in address
(Tenants → Sign-in → Domains). Browsers send `*.localhost` names to your
own machine, so binding `acme.localhost` to a tenant and opening
`http://acme.localhost:18080` tries it out.
