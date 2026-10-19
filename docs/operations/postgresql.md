# PostgreSQL

metastratum needs one writable PostgreSQL database (tested with 17 and 18),
and relies on three things that rule out some setups:

- **Session-level advisory locks** (leader election) and **LISTEN/NOTIFY**
  (live updates): connection poolers in transaction mode (PgBouncer's
  `pool_mode = transaction`) break both. Connect directly, or through a
  proxy that keeps a client on one server connection for good (HAProxy,
  PgBouncer in session mode).
- **Read-your-writes**: everything goes to the primary; there is no use for
  read replicas.
- About 20 connections per controller replica.

A recipe that fits: three PostgreSQL servers managed by
[Patroni](https://patroni.readthedocs.io/) (with etcd, or the
Kubernetes API, as its store), synchronous replication to one standby
(`synchronous_mode: true`) so a failover loses no committed task, and
HAProxy in front sending connections to whichever node Patroni's REST API
reports as primary (`option httpchk GET /primary`). Point `database_url` at
HAProxy.

On failover the controllers' connections break. They reconnect on their
own: tasks that were mid-step resume from their last recorded step, the
leader loses its lock and one is elected again, and the live-update
listener reconnects and tells browsers to refresh. Expect API errors for
the seconds the failover takes, and nothing else.

Back up the database as usual (`pg_basebackup` plus WAL archiving, or
pgBackRest). A restore rolls metastratum back to that moment: VMs created
since are unknown to it (their domains are left alone on the hosts, but
will need cleaning up by hand).
