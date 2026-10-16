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
| Hosts | their own VMs, and, through libvirt, the other hosts (see below) |
| Controller replicas | everything; they hold the master key |

The boundaries hypermon enforces:

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
  defence against cross-site request forgery) and, with `--secure-cookies`,
  `Secure`. Sessions end after 12
  hours, at sign-out, or when the user's password changes or the account is
  disabled.
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
  over, with TLS and host certificates. The controller reaches
  OVN's northbound database over TLS with a client certificate; hosts reach
  the southbound one with certificates from a CA of its own, naming their
  chassis, under OVN's RBAC.
- **Abuse**: requests and password sign-ins are rate limited per client,
  and failed sign-ins per account (see [operations](docs/operations.md#rate-limits)).
- **Browsers**: a Content-Security-Policy allowing scripts only from the
  controller, framing denied, `nosniff`, no referrers to other sites, and
  `Strict-Transport-Security` with `--secure-cookies`.
- **Audit**: every change made through the API, and every sign-in,
  successful or not, is recorded with who made it; the API offers no way to
  alter the record.

## Review, October 2026

A review of the whole system before calling it hardened. Fixed:

| Finding | Fix |
|---|---|
| Tenant image URLs could make hosts fetch from the management network and serve the response back as a disk; qcow2 images could name backing or external data files on the host | public addresses only, no file references, size and resource limits |
| Secrets (the CA key among them) were stored in plain text in the database | sealed under a master key |
| A sub-tenant admin could point their identity provider at an internal address and read the controller's errors, probing the management network | sub-tenants' providers, and the endpoints their discovery documents name, must be public; the provider tenant's, and issuers listed in `--trusted-oidc-issuers`, may be internal |
| The OVN northbound database was reachable over plain TCP only: anyone on the management network could rewrite all tenant networks | TLS with client certificates, and clustered databases |
| Replica certificates were accepted for their whole year, also after the replica was gone | only live replicas' certificates are accepted |
| No limit on sign-in attempts | rate limits per client and per account, shared by replicas through Redis |
| No browser security headers | CSP and the headers above |
| An agent from another version could be sent commands it does not know | protocol versions and capabilities |
| Hosts' certificates were valid for a year | 30 days, renewed automatically; longer ones are renewed at once |
| Hosts' libvirt accepted any certificate the controller's CA issued (replicas' too) | only host certificates (`tls_allowed_dn_list`, kept by the agent) |
| Host certificates could not be revoked | the controller's CA keeps a revocation list that hosts install for QEMU and libvirt: a renewed host's previous certificate is revoked once it uses the new one, a decommissioned host's at once |
| For live migration, each host's libvirt accepted connections from the other hosts with full control: one compromised host could take over the rest | the controller drives migrations on each host's own libvirt; libvirt's TLS listener is closed (opened only while an older agent in the cluster still migrates peer to peer) |
| API tokens carried all of their user's rights | tokens can be limited to roles at tenants |
| Hosts' ovn-controller used the southbound database with no TLS or RBAC: any host could rewrite every host's flows | agents set up TLS with a certificate per chassis from a separate OVN CA, under OVN's RBAC (`--ovn-sb`) |
| Unchecked whether IPv6 gets around IPv4-only networks | tested: guests reach no neighbour over IPv6, and cannot advertise themselves as router |

Not fixed, and worth knowing:

- **During a live migration**, the destination's QEMU accepts the incoming
  VM from any client certificate the controller's CA issued (hosts' and
  controller replicas'), not only the source's. Another host that is taken over could race to send a VM of
  its own instead, for the seconds a migration takes to start. Keep hosts
  off networks guests or tenants reach.
- **OVN cannot revoke certificates.** Open vSwitch has no revocation
  lists, so a decommissioned host's southbound certificate stays valid until
  it expires (up to 30 days), held by OVN's RBAC to that host's chassis.
  Delete the chassis (`ovn-sbctl chassis-del`) after decommissioning a host
  you no longer trust. Reinstall a host you no longer trust, and replace
  the CA if one was compromised (every host must then enroll again).
- **OVN's databases need TLS configured.** Without `--ovn-sb` and TLS on
  the northbound database, anyone who reaches them controls every
  tenant's network and every host's flows
  ([operations](docs/operations.md#ovn)).
- **Without Redis** (`--redis-url`), rate limits are per replica: N
  replicas allow N times the budget, and a restart resets it.
- **No limits on guests' disk or network bandwidth**: a VM can slow down
  others on the same host or storage.
- **Networking is IPv4 only.** Port security, security groups and
  guardrails are written for IPv4; guests' IPv6 is dropped.
- **Inline styles are allowed** by the CSP (the serial terminal and the UI
  set some); inline scripts are not.
- The controller serves plain HTTP; HTTPS is the job of the proxy in front
  of it. Without `--secure-cookies`, session cookies are sent over HTTP too.
