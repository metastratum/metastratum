# hypermon

A KVM-based virtual machine management platform: multiple hypervisor hosts,
OVN-backed virtual networks with ACLs, live migration, nested multi-tenancy and
a web UI with a virtual console.

## Layout

| Path | What |
|---|---|
| `crates/hm-core` | Shared domain types (IDs, tenant paths, task states) |
| `crates/hm-db` | Postgres repositories and migrations runner |
| `crates/hm-controller` | Control plane: REST API, scheduler, task engine, reconcilers |
| `crates/hm-agent` | Host agent: drives libvirt and OVS on each hypervisor |
| `crates/hmctl` | Admin CLI |
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
cargo run -p hm-controller        # serves http://127.0.0.1:8080/healthz
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
