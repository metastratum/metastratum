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
  values, stored only as hashes. Session cookies are `HttpOnly`,
  `SameSite=Strict` (so requests from other sites do not carry them: the
  defence against cross-site request forgery) and, with `--secure-cookies`,
  `Secure`. Sessions end after 12
  hours, at sign-out, or when the user's password changes or the account is
  disabled.
- **Sign-in through identity providers** uses the authorization code flow
  with PKCE, a nonce and a state value, verifies the ID token's signature,
  issuer and audience, and follows no redirects.
- **Secrets kept in the database** (the CA's private key, storage
  credentials, identity provider client secrets, BMC passwords) are sealed
  with XChaCha20-Poly1305 under a master key kept outside the database,
  bound to what they belong to. API responses never contain them.
- **Between components**: agents and controllers authenticate each other
  with mutual TLS under the controller's CA; certificates name what they
  belong to (hosts by ID, replicas by name), and are checked against the
  database (a host's current or just-renewed certificate; a live replica).
  Hosts' certificates last a year and are renewed automatically; their
  subjects are chosen by the controller, not taken from the request.
  Libvirt (live migration) uses TLS between hosts with the same
  certificates. The controller can reach OVN over TLS with a client
  certificate.
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
| No limit on sign-in attempts | rate limits per client and per account |
| No browser security headers | CSP and the headers above |
| An agent from another version could be sent commands it does not know | protocol versions and capabilities |

Not fixed, and worth knowing:

- **A compromised host can take over the other hosts.** For live migration,
  each host's libvirt accepts TLS connections from any certificate the
  controller's CA issued, with full control. Restricting libvirt to host
  certificates (`tls_allowed_dn_list`) would keep other holders of CA
  certificates out, but not another host: live migration needs that access.
  Treat hosts as one trust domain, and keep them off networks guests or
  tenants reach.
- **No certificate revocation.** A decommissioned host's certificate is
  refused by the controllers, but other hosts' libvirt accepts it until it
  expires (up to a year). Reinstall a host you no longer trust, and replace
  the CA if one was compromised (every host must then enroll again).
- **OVN southbound access** is not configured by hypermon. With plain TCP,
  anyone who reaches it controls every host's flows. Use TLS with OVN's
  RBAC ([operations](docs/operations.md#ovn)).
- **Rate limits are per replica**, in memory: N replicas allow N times the
  budget, and a restart resets it.
- **API tokens carry all of their user's rights**; there are no scopes yet.
  Give automation its own user with only the roles it needs.
- **No limits on guests' disk or network bandwidth**: a VM can slow down
  others on the same host or storage.
- **Networking is IPv4 only.** Port security, security groups and
  guardrails are written for IPv4.
- **Inline styles are allowed** by the CSP (the serial terminal and the UI
  set some); inline scripts are not.
- The controller serves plain HTTP; HTTPS is the job of the proxy in front
  of it. Without `--secure-cookies`, session cookies are sent over HTTP too.
