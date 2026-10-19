# OVN

Run the northbound and southbound databases as three-server Raft clusters
with `ovn-northd` beside them (`ovn-ctl --db-nb-cluster-local-addr ...`).
Give the controllers all three northbound servers; they use the leader and
follow it when it changes.

Use TLS for both: whoever can write the northbound database can rewrite
every tenant's network, and whoever can write the southbound one, the flows
on every host. The controller has the certificate authorities for both:

- **Northbound**, which only the controllers use, trusts a CA of its own
  that issues certificates to controllers only: the northbound database has
  no access control, so hosts must never get in. Each replica issues itself
  a client certificate from it at startup and renews it.
- **Southbound**, which every host's `ovn-controller` uses, trusts the OVN
  CA. With `ovn.southbound` set, each agent gets a certificate from it naming
  its chassis, installs it, points `ovn-controller` at the southbound remote,
  and renews it like its own. OVN's RBAC then holds each host to its own
  chassis: a host that is taken over cannot rewrite the others' bindings or
  flows. The chassis name is the host's Open vSwitch `system-id`; two live
  hosts cannot have the same one.

The OVN servers' certificates come as one bundle per OVN central node:

```sh
mst-controller ovn-certificate --name ovn1 --address 10.0.0.31 --out-dir ovn1   # on a controller
scp -r ovn1 root@10.0.0.31: && ssh root@10.0.0.31 sh ovn1/setup.sh
```

`setup.sh` installs the certificates in `/etc/ovn/metastratum`, configures
both databases to use them, and serves them over TLS: northbound on
`pssl:6641`, southbound on `pssl:6642` with OVN's RBAC (`--nb-port` and
`--sb-port` choose others), replacing plain listeners on those ports. In a
cluster the settings are shared, so the files must be at the same place on
every node: run each node's bundle on that node. Then start the controllers
with `ssl:` servers in `ovn.northbound` and `ovn.southbound`, nothing else.

The server certificates last a year (`--days`). The controllers look at
what each server presents every hour, export its expiry
(`mst_ovn_certificate_expiry_timestamp_seconds`), and warn in their log a
month ahead, or when a server presents a certificate from another CA. To
renew, make a new bundle and run its `setup.sh` again: the servers pick the
new files up without restarting.

`deploy/lab/lab.sh ovn-tls` installs the lab's bundle on `mst-ctl`.

OVN answers VMs' DHCP itself: their address, gateway and MTU, and the name
servers in the controllers' `[network] dns_servers` (the same on every
controller). Without them, VMs resolve no names unless their images bring
a resolver of their own. The servers must be reachable from the VMs: on
an external network their VPC routes to, or beyond it.
