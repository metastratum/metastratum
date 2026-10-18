# Testing

Three kinds of tests:

- **Unit and integration tests** run with `cargo test` and `npm test`.
- **End-to-end tests** drive a running controller and real hypervisor
  hosts through the public API (`tests/e2e`).
- **Browser tests** do the same through the web UI (`web/e2e`).

The last two run against the nested-KVM lab: [Development](development.md)
sets it up, and runs a controller against it.

## Unit and integration tests

They need PostgreSQL, which the dev compose file provides:

```sh
docker compose -f deploy/dev/compose.yaml up -d
export DATABASE_URL=postgres://metastratum:metastratum@127.0.0.1:55432/metastratum
cargo test
(cd web && npm test)
```

Some tests need more, and skip themselves without it:

| For | Give them |
|---|---|
| Domain XML checked against libvirt's schemas, disk images | `virt-xml-validate` and `qemu-img`, which come with libvirt and QEMU |
| A libvirt daemon (the tests create and remove small throwaway domains; your user needs `qemu:///system`) | `MST_TEST_LIBVIRT_SOCKET=/run/libvirt/virtqemud-sock` |
| Rate limits shared through Redis | `MST_TEST_REDIS_URL=redis://127.0.0.1:56379` (in the dev compose file) |
| OVN northbound databases nothing else uses: plain, over TLS, and a three-server cluster | `deploy/lab/lab.sh testdb` starts them on the lab and prints the `MST_TEST_OVN_NB_*` variables to export |

## End-to-end tests

Both suites skip unless they are given a controller to test:

```sh
export MST_E2E_URL=http://127.0.0.1:18080 MST_E2E_PASSWORD=...
cargo test -p mst-e2e -- --nocapture   # the API suites
(cd web && npm run e2e)                # the browser tests
```

They work in the controller's existing tenants and hosts: every test names
what it makes after itself (`e2e-<suite>-...`), and deletes it afterwards,
also when it fails.

### Running them on the lab

`deploy/lab/e2e.sh` runs the suites three at a time, which takes about 8
minutes rather than 25 one after another:

```sh
deploy/lab/e2e.sh                     # all of them
deploy/lab/e2e.sh balloon browser     # some, by name
deploy/lab/e2e.sh --changed           # those what changed since origin/main can affect
deploy/lab/e2e.sh --changed -n        # ...only saying which
deploy/lab/e2e.sh -j 2                # two at a time (more than 3 run the lab out of memory)
```

`--changed` looks at what changed, committed or not: a change to the web UI
runs the browser tests alone, documentation none, and a change to code
every suite shares (the VM engine, the agent) all of them.

Suites that change what the others rely on run first, on their own:
`storage` (it makes the NFS pool the others use), `limits` (it throttles
every VM of the cluster) and `m5` (it renews the hosts' certificates). The
browser tests run last, also on their own: they migrate a large VM, which
needs room on another host. Each suite's output goes to a log, and those of
the suites that failed are printed.

### Settings

| Variable | What it does |
|---|---|
| `MST_E2E_URL` | The controller to test (its API's address). Without it, nothing runs. |
| `MST_E2E_USER`, `MST_E2E_PASSWORD` | The provider administrator to sign in as (`admin` by default). |
| `MST_E2E_URL2` | A second replica: every replica must serve every host. |
| `MST_E2E_NFS` | The NFS export for a storage pool (default the lab's `10.77.0.10:/srv/nfs/metastratum`). |
| `MST_E2E_BACKUP_NFS` | The NFS export for backup repositories (default the lab's `10.77.0.10:/srv/nfs/backups`). |
| `MST_E2E_RBD_KEY` | The Ceph key `lab.sh ceph` prints: VMs, migrations, snapshots and clones on Ceph too. |
| `MST_E2E_DEX` | Dex's issuer (`http://127.0.0.1:5556/dex`): sign-in through a tenant's own identity provider. |
| `MST_E2E_YUBICO` | An address (`127.0.0.1:5557`) for a stand-in YubiKey OTP validation service the tests run; the controller's `[yubico]` must point at it, as `deploy/dev/controller.toml` does. |
| `MST_E2E_EVACUATE=1` | Also evacuate a host, which moves every VM on it, not only the tests'. |
| `MST_E2E_FENCE=1` | Freeze a lab host, wait for it to be fenced and its VM restarted on another, then start it again and return it to service. |
| `MST_E2E_OUTSIDE=1` | Check that a VM reaches the outside by name (needs the default VPC on an external network, as the installers set up). |
| `MST_E2E_IMAGE_URL` | The cloud image VMs boot (default Debian 13's). |
| `MST_E2E_DEBUG=1` | Print console traffic. |
| `MST_E2E_CHANNEL` | The browser the browser tests use (default `chrome`, your installed Chrome). |

### What they cover

| Suite | What it checks |
|---|---|
| `m1` | A VM's life on real hosts: both consoles (graphical and serial), a ping between VMs across the overlay network, power cycles, deletion. |
| `m2` | Nested tenants, inherited roles, hierarchical quotas, the audit log and API tokens. |
| `m3` | VPCs isolated from each other, outbound NAT, a floating IP that answers SSH only while a security group allows it, and a parent tenant's guardrail closing it again whatever the child's groups say. |
| `m4` | Live migration on local disk, NFS and Ceph, while another VM pings the migrating one and holds a TCP connection to it: the guest never restarts, and loses few packets. Cancelling a migration. |
| `m5` | Hardening: tenants' images (and identity providers) cannot reach internal addresses; every replica reaches every host; failed hosts are fenced and their VMs restart elsewhere; hosts renew their certificates and migrate with the new ones. |
| `storage` | VMs on a shared NFS pool, and granting pools to tenants. |
| `limits` | A cluster's disk and network limits, measured inside guests, and lifted again on running VMs. |
| `balloon` | Memory changed while a VM runs, through its balloon, and what the guest reports using. |
| `hotplug` | CPUs plugged in and out of a running VM. |
| `resize` | A running VM's disk grown (the guest sees it at once); a stopped VM's CPUs, memory and disk (its file system grows at boot). |
| `clone` | A stopped VM made a template, and clones of it booting with what its disks held, on local disk, NFS and Ceph. |
| `snapshots` | Snapshots taken of running and stopped VMs, reverted to and deleted, on local disk, NFS (where the VM migrates with them) and Ceph. |
| `backups` | Backups of running and stopped VMs, by hand and on a schedule, restored as new VMs. Chains of incremental ones (across a migration, of a stopped VM, after a snapshot revert) restore what the VM held, also after the backups they built on are merged into them. |
| `disks` | Disks plugged into running VMs (the guest finds them by ID) and out again, grown, moved along when a VM migrates, kept when it is deleted, and moved between hosts and into NFS, intact each time. |
| `cpu` | CPU models every host of a cluster runs, fitted again on request. |
| `load_balancers` | An external address spreading connections over VMs and leaving out one that stops answering, an internal one, changed members. |
| `events` | Live updates reach those who may see the change, and only them. |
| `domains` | A tenant's own sign-in address. |
| `oidc` | Signing in through a tenant's identity provider (Dex), and groups becoming roles. |
| `two_factor` | Authenticator apps, security keys, backup codes, YubiKey OTPs, a tenant requiring a second factor, administrators resetting one. |
| `browser` | The web UI with Playwright: a VM's life with both consoles, migration, snapshots, disks, resizing and templates; tenants, people and quotas; networks; live updates; confirmations before destroying; two-factor sign-in; backups. |
