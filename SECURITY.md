# Security

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's private
vulnerability reporting for this repository, rather than in a public issue.

## Who is trusted with what

| Who | Trusted with |
|---|---|
| Provider administrators (admins of the root tenant) | everything: hosts, storage, external networks, every tenant |
| Tenant administrators | their tenant and its sub-tenants, nothing above or beside it |
| Tenant users | what their roles allow in their tenant's subtree |
| Guests (the VMs) | nothing; they are assumed hostile |
| Hosts | their own VMs; they can disturb, but not take over, migrations between other hosts (see below) |
| Controller replicas | everything; they hold the master key |

The boundaries metastratum enforces:

- **Between tenants.** Every API call is authorized against the caller's
  role bindings, which apply to the tenant they are made at and its
  sub-tenants. Resources referred to in a request (networks, security
  groups, images, storage pools, floating IPs) must belong to, or be
  granted to, the tenant acting. Live updates (`/api/v1/events`) only carry
  changes the subscriber may see. A sub-tenant's quotas and grants never
  exceed its parent's.
- **Between guests.** Each NIC's OVN port only sends from its own MAC and
  IP (port security). Ports drop everything their security groups do not
  allow, and guardrails set by ancestor tenants drop traffic no security
  group can let through. VPCs are separate routers; the same CIDR in two
  VPCs never meets. QEMU runs each VM in its own process, confined by
  libvirt's per-VM AppArmor (or SELinux) profile.
- **Between guests and the platform.** Consoles are reached only through
  the controller: VNC listens on a unix socket on the host, never on TCP.
  A console session needs a ticket that is single-use, valid for 30
  seconds, and tied to the user and VM it was issued for.
- **Between tenants' input and the infrastructure.** Images that tenants
  add are fetched only from public internet addresses (checked when
  connecting, so redirects and DNS tricks do not get around it), capped in
  size, and refused if they refer to other files (backing files, external
  data files); `qemu-img` runs under resource limits. Identity providers
  configured by sub-tenants likewise must be on public addresses.

## How it is protected

- **Passwords** are hashed with Argon2id; sign-in takes as long whether or
  not the account exists. **Sessions and API tokens** are random 256-bit
  values, stored only as hashes. A token can be limited to roles at
  tenants below what its user holds; it then also cannot change its user's
  account or make other tokens. Session cookies are `HttpOnly`,
  `SameSite=Strict` (so requests from other sites do not carry them: the
  defence against cross-site request forgery) and `Secure` when a trusted
  proxy says the client came over HTTPS. Sessions end after 12
  hours, at sign-out, or when the user's password changes or the account is
  disabled.
- **Tenant domains** are bound by provider administrators only: they
  decide which names lead to the controllers, and a tenant must not take
  over another's sign-in page, or the provider's. A tenant with a domain,
  and its sub-tenants, can be signed in to only there (up to a sub-tenant
  with a domain of its own); from any other address they look like tenants
  that don't exist. The sign-in page at a domain tells anyone the tenant's
  name and whether it has sub-tenants to sign in to there (never their
  names); elsewhere, only whether the provider has any.
- **Sign-in through identity providers** uses the authorization code flow
  with PKCE, a nonce and a state value, verifies the ID token's signature,
  issuer and audience, and follows no redirects.
- **Secrets kept in the database** (the CAs' private keys, storage
  credentials, identity provider client secrets, BMC passwords) are sealed
  with XChaCha20-Poly1305 under a master key kept outside the database,
  bound to what they belong to. API responses never contain them.
- **Between components**: agents and controllers authenticate each other
  with mutual TLS under the controller's CA; certificates name what they
  belong to (hosts by ID, replicas by name), and are checked against the
  database (a host's current or just-renewed certificate; a live replica).
  Hosts' certificates last 30 days and are renewed automatically; their
  subjects are chosen by the controller, not taken from the request.
  Replaced and decommissioned hosts' certificates are revoked: the
  controllers refuse them, and so do the other hosts' QEMU and libvirt,
  which install the CA's revocation list.
  Hosts' libvirt does not listen on the network: the controller drives
  live migrations on each host's own libvirt, and only QEMU sends the VM
  over, with TLS. Each migration gets certificates of its own, from a CA
  made for it alone: the destination accepts the VM from that source
  only, and the source sends it to that destination only. The controller reaches
  OVN's northbound database over TLS with a client certificate from a CA
  that issues to controllers only; hosts reach the southbound one with
  certificates from another CA, naming their chassis, under OVN's RBAC.
  The controllers issue the OVN servers' certificates too, and warn before
  they expire.
- **Abuse**: requests and password sign-ins are rate limited per client,
  and failed sign-ins per account (see [operations](docs/operations.md#rate-limits)).
- **Browsers**: a Content-Security-Policy allowing scripts only from the
  controller, framing denied, `nosniff`, no referrers to other sites, and
  `Strict-Transport-Security` on requests that came over HTTPS.
- **Audit**: every change made through the API, and every sign-in,
  successful or not, is recorded with who made it; the API offers no way to
  alter the record. So are the cleanups that follow deletions.
- **Leaving nothing behind**: deleting a pool or an image, or decommissioning
  a host, has the hosts let go of mounts, credentials and cached data
  (hosts that were down do so when they reconnect); Delete, rather than
  Unlink, also removes metastratum's data from the storage, or metastratum from
  the machine.
- **Dependencies** are checked against the RustSec and npm advisory
  databases (`cargo audit`, `npm audit`). One advisory is recorded as not
  applying, with the reason, in `.cargo/audit.toml`.

## Review, October 2026

A review of the whole system before calling it hardened. Fixed:

| Finding | Fix |
|---|---|
| Tenant image URLs could make hosts fetch from the management network and serve the response back as a disk; qcow2 images could name backing or external data files on the host | public addresses only, no file references, size and resource limits |
| Secrets (the CA key among them) were stored in plain text in the database | sealed under a master key |
| A sub-tenant admin could point their identity provider at an internal address and read the controller's errors, probing the management network | sub-tenants' providers, and the endpoints their discovery documents name, must be public; the provider tenant's, and issuers listed in `api.trusted_oidc_issuers`, may be internal |
| The OVN northbound database was reachable over plain TCP only: anyone on the management network could rewrite all tenant networks | TLS with client certificates, and clustered databases |
| Replica certificates were accepted for their whole year, also after the replica was gone | only live replicas' certificates are accepted |
| No limit on sign-in attempts | rate limits per client and per account, shared by replicas through Redis |
| No browser security headers | CSP and the headers above |
| An agent from another version could be sent commands it does not know | protocol versions and capabilities |
| Hosts' certificates were valid for a year | 30 days, renewed automatically; longer ones are renewed at once |
| Hosts' libvirt accepted any certificate the controller's CA issued (replicas' too) | only host certificates (`tls_allowed_dn_list`, kept by the agent) |
| Nothing kept a VM from taking all of a host's disk or network bandwidth | per-VM disk (IOPS, MB/s) and network (Mbit/s each way) limits per cluster, enforced by QEMU and OVN, changed live |
| The destination of a live migration accepted the incoming VM from any certificate the controller's CA issued: another host could send it a VM of its own instead of the source's | each migration gets a CA and certificates of its own; QEMU on each side trusts only that CA while it sets up the migration's TLS |
| Host certificates could not be revoked | the controller's CA keeps a revocation list that hosts install for QEMU and libvirt: a renewed host's previous certificate is revoked once it uses the new one, a decommissioned host's at once |
| For live migration, each host's libvirt accepted connections from the other hosts with full control: one compromised host could take over the rest | the controller drives migrations on each host's own libvirt; libvirt's TLS listener is closed (opened only while an older agent in the cluster still migrates peer to peer) |
| API tokens carried all of their user's rights | tokens can be limited to roles at tenants |
| Hosts' ovn-controller used the southbound database with no TLS or RBAC: any host could rewrite every host's flows | agents set up TLS with a certificate per chassis from a separate OVN CA, under OVN's RBAC (`ovn.southbound`) |
| Unchecked whether IPv6 gets around IPv4-only networks | tested: guests reach no neighbour over IPv6, and cannot advertise themselves as router |
| Deleting a pool, an image or a host left mounts, storage credentials (Ceph keyrings, libvirt secrets) and cached data on hosts | hosts let go of them at once, or when they reconnect; Delete also removes metastratum's data from the storage or the machine |
| A dependency (`inout` 0.2.3) had been yanked | updated to 0.2.4 |

## Known issues, and what to do about them

- **Without TLS, OVN's databases are open.** Whoever reaches the northbound
  database can rewrite every tenant's network; whoever reaches the
  southbound one, every host's flows. metastratum supports TLS for both but
  cannot impose it on OVN.
  *Mitigation:* install a bundle from `mst-controller ovn-certificate` on
  each OVN central node, which switches both databases to TLS, and run the
  controllers with `ssl:` servers in `ovn.northbound` and `ovn.southbound`; remove any
  plain listeners left ([operations](docs/operations.md#ovn)).
- **The controller serves plain HTTP.** Sessions and passwords would cross
  the network in clear text.
  *Mitigation:* put a TLS-terminating proxy in front of it, listen on
  localhost or a private address only, list the proxy in
  `api.trusted_proxies` and have it set `X-Forwarded-Proto` (then cookies are
  `Secure`, HSTS is on and OIDC redirect URIs use `https`).
- **A compromised controller compromises everything.** It holds the master
  key, both CAs and the database credentials, and drives every host.
  *Mitigation:* treat controllers like the CA they are: dedicated machines,
  the hardened systemd unit in `deploy/systemd`, the master key and keys
  passed as systemd credentials readable by the service only, the master
  key kept out of the database's backups (backed up separately), and the
  database reachable by the controllers only, over TLS.
- **Local accounts have no second factor or idle timeout.** A session lasts
  12 hours unless its user signs out.
  *Mitigation:* sign people in through an identity provider that enforces
  multi-factor authentication (per tenant, and for the provider tenant
  too), and keep local accounts for break-glass use with long passwords.
  Give automation scoped API tokens that expire.
- **Rate limits are per controller without Redis.** N controllers allow N
  times the sign-in attempts, and a restart resets them.
  *Mitigation:* share them through Redis (`api.redis_url`, `rediss://` for
  TLS).
- **A host may be able to disrupt a migration it is not part of.** It
  cannot send a VM in: the destination accepts only the source's one-off
  certificate (tried on the lab: QEMU refused another host's, then took
  the source's). By occupying the destination's migration port first and
  stalling, it might make the migration fail; the migration would be
  undone, the VM still running on its source.
  *Mitigation:* keep hosts on a management network that guests and tenants
  cannot reach, and let only hosts reach the migration ports
  (49152-49215).
- **OVN cannot revoke certificates.** Open vSwitch has no revocation
  lists, so a decommissioned host's southbound certificate stays valid
  until it expires (up to 30 days), held by OVN's RBAC to that host's
  chassis. OVN is the admin's, so decommissioning leaves the chassis.
  *Mitigation:* delete the chassis (`ovn-sbctl chassis-del`) when a machine
  leaves OVN too. Reinstall a host you no longer trust, and replace the CA
  if one was compromised (every host must then enroll again).
- **Tenants' own cloud-init user data is stored as given**, in the
  database, on the host's seed image and in the guest. (Passwords given as
  such are hashed before they are stored; the API never returns user
  data.)
  *Mitigation:* put no secrets in user data; use SSH keys or the password
  field.
- **Per-VM limits are opt-in.** Without them, a VM can slow down others on
  the same host or storage; CPU and memory are shared by the overcommit
  ratios alone.
  *Mitigation:* set disk and network limits on each cluster
  ([operations](docs/operations.md#limits)), and keep overcommit modest.
- **Networking is IPv4 only.** Port security, security groups and
  guardrails are written for IPv4; guests' IPv6 is dropped (tested), not
  filtered.
- **Inline styles are allowed** by the Content-Security-Policy (the serial
  terminal and the UI set some); inline scripts are not.

## Deployment checklist

What metastratum offers but leaves to the deployment to switch on:

- HTTPS in front of the controllers, with `X-Forwarded-Proto` from a proxy
  in `api.trusted_proxies`; the API listening on a private address.
- TLS for OVN's databases: a bundle from `ovn-certificate` on each OVN
  central node, `ssl:` servers in `ovn.northbound` and `ovn.southbound`, and no plain
  listeners left.
- `api.redis_url` with more than one controller.
- The master key and other keys as systemd credentials; the master key out
  of the database's backups.
- PostgreSQL over TLS (`sslmode=verify-full`), reachable by the
  controllers only.
- A management network for hosts and controllers that guests and tenants
  cannot reach; migration ports (49152-49215) open between hosts only;
  `metrics_listen` on a private address.
- Fencing on every host of clusters with high availability.
- Per-VM limits on clusters.
- Sign-in through identity providers with multi-factor authentication;
  scoped, expiring API tokens for automation.
- `cargo audit` and `npm audit` with every update.
