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

```sh
docker compose -p hypermon -f deploy/dev/compose.yaml up -d
export DATABASE_URL=postgres://hypermon:hypermon@127.0.0.1:55432/hypermon
cargo test
cargo run -p hm-controller        # serves http://127.0.0.1:8080/healthz
```

Building needs `libvirt` development headers and `protoc`.

## Lab

`deploy/lab/lab.sh up` builds a four-VM lab on `qemu:///system` (needs the
libvirt daemons running, membership of the `libvirt` group and nested KVM).
`lab.sh ssh hm-host1` logs in; `lab.sh down` removes everything but the
cached base image.
