# hypermon

A KVM-based virtual machine management platform: multiple hypervisor hosts,
OVN-backed virtual networks with ACLs, live migration, nested multi-tenancy and
a web UI with a virtual console.

## Layout

| Path | What |
|---|---|
| `crates/hm-core` | Shared domain types (IDs, tenant paths, VM specs) |
| `crates/hm-db` | Postgres repositories |
| `crates/hm-pki` | Internal CA, certificate issuance, join tokens |
| `crates/hm-proto` | gRPC protocol between controller and agents (`proto/agent.proto`) |
| `crates/hm-libvirt` | Domain XML and an async libvirt facade |
| `crates/hm-ovsdb` | OVSDB client and OVN northbound operations |
| `crates/hm-storage` | Host storage pools (local qcow2 for now) |
| `crates/hm-controller` | Control plane: REST API, agent endpoint, task engine, console relay |
| `crates/hm-agent` | Host agent: drives libvirt, storage and consoles on each hypervisor |
| `crates/hmctl` | Admin CLI (placeholder) |
| `web/` | Web UI (Vue 3 + TypeScript) |
| `tests/e2e` | End-to-end tests against a running controller and lab |
| `migrations/` | SQL migrations (sqlx) |
| `deploy/dev` | Local dev dependencies (Postgres) |
| `deploy/lab` | Nested-KVM lab: 1 control VM + 3 hypervisor VMs |

## Development

Building needs a Rust toolchain, the libvirt development headers and `protoc`.
The tests need Postgres, which `deploy/dev/compose.yaml` provides in Docker:

```sh
sudo pacman -S --needed rustup libvirt protobuf docker   # Arch; Debian: libvirt-dev protobuf-compiler
docker compose -p hypermon -f deploy/dev/compose.yaml up -d
export DATABASE_URL=postgres://hypermon:hypermon@127.0.0.1:55432/hypermon
cargo test
```

Some tests need extra tools and skip themselves without them: `virt-xml-validate`
and `qemu-img` (both ship with libvirt and QEMU), or a reachable OVN northbound
database (`HM_TEST_OVN_NB=tcp:10.77.0.10:6641` with the lab running).

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
| `hm-ctl` | 10.77.0.10 | Postgres, OVN central (NB/SB, northd), NFS export |
| `hm-host1..3` | 10.77.0.11–13 | Hypervisors: libvirt/QEMU, OVS, ovn-controller |

It needs about 21 GiB of RAM, 14 vCPUs and roughly 10 GB of disk in
`/var/lib/libvirt/images/hm-lab`.

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
deploy/lab/lab.sh ssh hm-host1 [cmd...]
deploy/lab/lab.sh down      # remove VMs and networks; keeps the cached base image
```

`up` checks the requirements above first. Once the VMs boot, it verifies
internet access through NAT and that every hypervisor VM has a usable
`/dev/kvm`. If anything fails, it stops with the fix.

### Running hypermon against the lab

The controller runs on your machine and is reachable by the lab hosts as
`10.77.0.1`, the host side of the lab network. It uses the local dev Postgres
and the lab's OVN.

```sh
export DATABASE_URL=postgres://hypermon:hypermon@127.0.0.1:55432/hypermon
cargo build -p hm-controller
(cd web && npm run build)
HM_ADMIN_PASSWORD='choose-a-long-password' target/debug/hm-controller bootstrap
target/debug/hm-controller serve --listen 127.0.0.1:18080 \
  --agent-listen 10.77.0.1:8443 --agent-names 10.77.0.1 \
  --ovn-nb tcp:10.77.0.10:6641 --web-dir web/dist
```

Then build and install the agent on the three hosts. The script builds in a
Debian 13 container, so Docker is required, and enrolls each host with a fresh
join token:

```sh
DATABASE_URL=... deploy/lab/agent.sh
```

Open http://127.0.0.1:18080 and sign in as `admin`. Add an image (for example
`https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2`)
before creating VMs.

### End-to-end tests

Both suites run against the controller above and skip unless pointed at it:

```sh
HM_E2E_URL=http://127.0.0.1:18080 HM_E2E_PASSWORD=... cargo test -p hm-e2e -- --nocapture
cd web && HM_E2E_URL=http://127.0.0.1:18080 HM_E2E_PASSWORD=... npm run e2e
```

The Rust suite creates two VMs, checks both consoles, pings across the overlay
network between hosts, power-cycles and deletes them. `HM_E2E_DEBUG=1` prints
console traffic. The browser suite does the same through the UI with Playwright,
using your installed Chrome.
