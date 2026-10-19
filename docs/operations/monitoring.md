# Monitoring

`metrics_listen` serves Prometheus metrics at `/metrics`, on an address of
its own rather than the API's: they name hosts and count resources across
tenants. Scrape every replica:

```yaml
scrape_configs:
  - job_name: metastratum
    static_configs:
      - targets: ['ctl1.example.net:7822', 'ctl2.example.net:7822']
```

Counters (`mst_*_total`, histograms `mst_*_seconds`) are what that replica
did; sum them over replicas. Gauges about the platform (`mst_vms`,
`mst_host_up`, `mst_tasks_open`, ...) are read from the database, so every
replica reports the same values: use `max`.

| Metric | What |
|---|---|
| `mst_http_requests_total{method,route,status}`, `mst_http_request_duration_seconds` | API requests |
| `mst_tasks_finished_total{kind,outcome}`, `mst_task_duration_seconds{kind}` | tasks run |
| `mst_tasks_open{kind,state}` | tasks waiting or running |
| `mst_agent_commands_total{command,outcome}`, `mst_agent_command_duration_seconds` | commands sent to agents |
| `mst_agent_sessions` | agent sessions on this replica |
| `mst_host_up{host,state}` | whether each host's agent is connected and heartbeating |
| `mst_host_cert_expiry_timestamp_seconds{host}`, `mst_host_last_seen_timestamp_seconds{host}` | |
| `mst_vms{status,power}`, `mst_migrations_running` | |
| `mst_network_passes_total{outcome}`, `mst_network_changes_total{op}`, `mst_network_pass_duration_seconds` | OVN reconciliation |
| `mst_port_binding_mismatches` | VMs' ports OVN has bound to a host other than theirs (leader only; with `ovn.southbound`) |
| `mst_logins_total{method,outcome}`, `mst_rate_limited_total`, `mst_rate_limit_store_errors_total` | |
| `mst_fencings_total{outcome}`, `mst_leader`, `mst_replicas` | HA |

Alerts worth having:

```yaml
groups:
  - name: metastratum
    rules:
      - alert: MetastratumHostDown
        expr: max by (host) (mst_host_up{state=~"ready|maintenance"}) == 0
        for: 2m
      - alert: MetastratumNoLeader
        expr: sum(mst_leader) == 0
        for: 2m
      - alert: MetastratumNetworkReconcileFailing
        expr: sum(increase(mst_network_passes_total{outcome="failed"}[10m])) > 3
      - alert: MetastratumFencingFailed
        expr: sum(increase(mst_fencings_total{outcome="failed"}[15m])) > 0
      - alert: MetastratumHostCertificateExpiring
        expr: min by (host) (mst_host_cert_expiry_timestamp_seconds) - time() < 30 * 86400
      - alert: MetastratumTasksPiling
        expr: sum(max by (kind) (mst_tasks_open{state="pending"})) > 50
        for: 10m
      - alert: MetastratumLoginFailures
        expr: sum(rate(mst_logins_total{outcome="failed"}[5m])) > 1
        for: 10m
      # A host claiming another's VM ports: it may have been taken over.
      # The audit log (network.port_binding_mismatch) says which.
      - alert: MetastratumPortBoundElsewhere
        expr: max(mst_port_binding_mismatches) > 0
        for: 3m
```

Logs go to stderr (journald under systemd); `RUST_LOG` sets the level, e.g.
`RUST_LOG=info,mst_controller=debug`.
