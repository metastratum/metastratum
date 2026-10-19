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
| Hosts | their own VMs; they can disturb, but not take over, migrations between other hosts, and can claim other hosts' VM ports in OVN, which the controller reports (see below) |
| Controller replicas | everything; they hold the master key |

The boundaries metastratum enforces:

- **Between tenants.** Every API call is authorized against the caller's
  role bindings, which apply to the tenant they are made at and its
  sub-tenants. Resources referred to in a request (networks, security
  groups, images, storage pools, floating IPs) must belong to, or be
  granted to, the tenant acting. Live updates (`/api/v1/events`) only carry
  changes the subscriber may see. A sub-tenant's quotas and grants never
  exceed its parent's. People hold roles only at their own tenant or below
  it, where their account (its password, second factors, deletion) is
  managed: no tenant's administrators manage an account holding a role
  above or beside them.
- **Between guests.** Each NIC's OVN port only sends from its own MAC and
  IP (port security). Ports drop everything their security groups do not
  allow, and guardrails set by ancestor tenants drop traffic no security
  group can let through. Tenants' networks never overlap external
  networks' ranges, so no address a tenant hands out can be another's
  public one. Only DHCP, to the network's DHCP server alone,
  and OVN's own load balancer health checks (by OVN's MAC address, which
  guests cannot send from) get past them. VPCs are separate routers; the same CIDR in two
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
  data files) or describe a disk larger than the host's largest download
  (checked before unpacking); `qemu-img` runs under resource limits, its output
  capped at that size. An image is downloaded once, as it is added (two
  of a tenant's at a time, with time limits); hosts copy it from each
  other from then on, each copy checked against the digest of the disk
  first downloaded, as is a download again. VMs' disks are copies of
  their own. Identity providers
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
- **Second factors** for password accounts: authenticator apps (TOTP,
  each code accepted once), security keys and passkeys over WebAuthn (FIDO2
  and U2F keys, bound to the address they were added at, with signature
  counters checked), YubiKey OTPs checked by a Yubico validation service
  over signed requests, and single-use backup codes. TOTP secrets are
  sealed with the master key; backup codes are kept hashed. The second step
  of a sign-in expires after five minutes or five wrong answers, and wrong
  answers count against the account's sign-in budget. Adding or removing a
  factor, or changing one's password, takes the current password; adding
  one ends the user's other sessions, so none opened with the password
  alone outlives it. Accounts signing in through an identity provider
  cannot give themselves a password. A tenant
  can require a second factor of its password accounts and those below it;
  a session without one can then only add one.
- **Tenant domains** are bound by provider administrators only: they
  decide which names lead to the controllers, and a tenant must not take
  over another's sign-in page, or the provider's. A tenant with a domain,
  and its sub-tenants, can be signed in to only there (up to a sub-tenant
  with a domain of its own); from any other address they look like tenants
  that don't exist. The sign-in page at a domain tells anyone the tenant's
  name and whether it has sub-tenants to sign in to there (never their
  names); elsewhere, only whether the provider has any.
- **Sign-in through identity providers** uses the authorization code flow
  with PKCE, a nonce and a state value bound to the browser that started it
  (a cookie of its own), verifies the ID token's signature, issuer and
  audience, follows no redirects, and returns only to paths on the site.
- **Secrets kept in the database** (the CAs' private keys, storage
  credentials, identity provider client secrets, BMC passwords) are sealed
  with XChaCha20-Poly1305 under a master key kept outside the database,
  bound to what they belong to. API responses never contain them.
- **Between components**: agents and controllers authenticate each other
  with mutual TLS under the controller's CA; certificates name what they
  belong to (hosts by ID, replicas by name), and are checked against the
  database (a host's current or just-renewed certificate; a live replica).
  Hosts' certificates last 30 days and are renewed automatically; their
  subjects are chosen by the controller, not taken from the request, and
  the host names they carry can never be the controllers' own. A renewal
  revokes any other certificate the host held but did not use. What hosts
  report about themselves that would give them more (a new name, address
  or chassis, physical networks, more CPUs or memory) waits for a provider
  administrator to accept it.
  Replaced and decommissioned hosts' certificates are revoked: the
  controllers refuse them, and so do the other hosts' QEMU and libvirt,
  which install the CA's revocation list.
  Hosts' libvirt does not listen on the network: the controller drives
  live migrations on each host's own libvirt, and only QEMU sends the VM
  over, with TLS. Each migration gets certificates of its own, from a CA
  made for it alone: the destination accepts the VM from that source
  only, and the source sends it to that destination only. The destination
  defines the VM from the controller's description of it, never from what
  the source sends; agents from before this are refused. A disk moving
  between hosts is served read-only by `qemu-nbd` over TLS, with a CA and
  certificates made for that copy alone: only the host it moves to can read
  it, and the server stops after one copy (or an hour). The controller reaches
  OVN's northbound database over TLS with a client certificate from a CA
  that issues to controllers only; hosts reach the southbound one with
  certificates from another CA, naming their chassis, under OVN's RBAC
  (which keeps a host's writes to its own chassis, but not to its own
  ports: see the known issues).
  The controllers issue the OVN servers' certificates too, and warn before
  they expire.
- **Abuse**: requests and password sign-ins are rate limited per client,
  and failed sign-ins per account (a sign-in takes from the account's budget
  before it is checked, and gives back on success, so parallel guesses
  cannot all slip in before the first fails) (see [operations](docs/operations.md#rate-limits)).
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
| Hosts' certificates were valid for a year | 30 days, renewed automatically |
| Hosts' libvirt accepted any certificate the controller's CA issued (replicas' too) | libvirt no longer listens on the network at all |
| Nothing kept a VM from taking all of a host's disk or network bandwidth | per-VM disk (IOPS, MB/s) and network (Mbit/s each way) limits per cluster, enforced by QEMU and OVN, changed live |
| The destination of a live migration accepted the incoming VM from any certificate the controller's CA issued: another host could send it a VM of its own instead of the source's | each migration gets a CA and certificates of its own; QEMU on each side trusts only that CA while it sets up the migration's TLS |
| Host certificates could not be revoked | the controller's CA keeps a revocation list that hosts install for QEMU: a renewed host's previous certificate is revoked once it uses the new one, a decommissioned host's at once |
| For live migration, each host's libvirt accepted connections from the other hosts with full control: one compromised host could take over the rest | the controller drives migrations on each host's own libvirt; libvirt's TLS listener is closed |
| API tokens carried all of their user's rights | tokens can be limited to roles at tenants |
| Hosts' ovn-controller used the southbound database with no TLS or RBAC: any host could rewrite every host's flows | agents set up TLS with a certificate per chassis from a separate OVN CA, under OVN's RBAC (`ovn.southbound`) |
| Unchecked whether IPv6 gets around IPv4-only networks | tested: guests reach no neighbour over IPv6, and cannot advertise themselves as router |
| Deleting a pool, an image or a host left mounts, storage credentials (Ceph keyrings, libvirt secrets) and cached data on hosts | hosts let go of them at once, or when they reconnect; Delete also removes metastratum's data from the storage or the machine |
| A dependency (`inout` 0.2.3) had been yanked | updated to 0.2.4 |

## Second review, October 2026

Independent reviewers, each on one area and without this document, read
the code after two-factor sign-in was added. Fixed:

| Finding | Fix |
|---|---|
| Load balancers' health checks were let past every tenant's security groups and guardrails by their source address, which tenants could choose (any range for their networks): one tenant could reach every other's VMs from an address of its own | that rule is gone: OVN lets its own health checks through by its service monitor MAC address, which guests cannot send from |
| A host taken over could answer a migration away from it with domain XML of its own choosing (other VMs' disks, host devices) and the destination would run it | the destination defines the VM from the controller's description, also its persistent definition afterwards |
| A host claiming an old agent protocol made every peer in its cluster open libvirt's TLS listener to all host certificates | agents older than the first release's protocol are refused, before anything they say is kept; the listener is never opened |
| A host could name itself `mst-controller` and renew its certificate into one that agents and replicas took for the controller's | host names must be host names, never the controllers'; certificates leave out any other |
| A session opened with the password alone, before the user's first second factor, became a full session when the factor was added | adding a factor ends the user's other sessions |
| A tenant's administrators could reset the password or second factors of their own users who held roles above or beside the tenant, and take those roles over | roles only at the user's own tenant or below (enforced in the database too; roles held elsewhere were removed) |
| DHCP was let past guardrails to any address: UDP 68 to 67 as a way out | to the network's DHCP server only |
| Parallel sign-in attempts all passed the per-account limit before any of them failed | the account's budget is taken first and given back on success |
| Accounts signing in through an identity provider could give themselves a password, getting around its MFA and off-boarding | refused |
| Tenants' networks could overlap external networks' ranges, putting addresses such as another tenant's floating IP or the external gateway on a tenant's router | refused, checked from both sides in the database |
| A small qcow2 image could unpack into a huge one (compressed clusters), and cached images counted against nothing: a tenant could fill hosts' disks | images describing disks larger than the host's largest download are refused before unpacking, the conversion's output is capped, and images count against an `image_gib` quota (their stored size) |
| Snapshots, and VPCs (each taking an address from an external network's pool), counted against no quota | `snapshot_gib` (each at its VM's disk size) and `vpcs` quotas |
| Hosts' own reports of their physical networks, capacity, name, address and chassis were taken as they came: a host taken over could make itself the gateway of other tenants' VPCs, or draw their VMs | taken at enrollment; later changes that would give the host more wait for an administrator |
| A host could renew its certificate any number of times, and certificates replaced before use were never revoked | a renewal revokes any other certificate the host held but did not use |
| An OIDC sign-in was not bound to the browser that started it: someone could hand another person the link back from their own identity provider and sign them in as themselves | a cookie of its own binds it |
| `return_to=/%09/evil.example` passed the filter, as browsers drop tabs and newlines from URLs: an open redirect after sign-in | no control characters or spaces, on the sign-in page too |
| A guardrail a tenant set on a sub-tenant belonged to the sub-tenant, whose administrators could remove it | guardrails belong to the tenant that sets them, which alone removes them |
| An administrator resetting a compromised account's password or second factors left its API tokens working | an option to revoke them too (off by default: a plain reset should not break someone's scripts) |
| VMs' disks, cached images and cloud-init seeds (holding user data) were world-readable on hosts | readable by root and the VM's own QEMU only; the agent warns when libvirt confines no VM |
| The Terraform provider showed `user_data` in plans and logs | marked sensitive |
| The sign-in page showed error text from its address, which anyone could put there | error codes, in the page's own words |
| Live updates told sub-tenants' users about their ancestors' unshared images | only shared ones |
| SECURITY.md claimed OVN's RBAC held hosts to their own chassis; it does not keep them off other hosts' ports | corrected (known issues), and the controller watches port bindings |

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
- **Sessions have no idle timeout.** A session lasts 12 hours unless its
  user signs out.
  *Mitigation:* require a second factor (Tenants → Sign-in), or sign
  people in through an identity provider with its own; give automation
  scoped API tokens that expire.
- **API tokens skip the second factor.** A token is made in a session that
  had one, but signs in alone, also once a tenant starts requiring second
  factors.
  *Mitigation:* scoped tokens that expire, revoked when their owner leaves.
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
- **A host can claim other hosts' VM ports in OVN.** OVN's RBAC lets
  every host write any port binding's chassis, so a host taken over could
  draw another tenant's traffic to itself; ovn-controller on the right
  host claims it back, so traffic flaps rather than stays.
  *Mitigation:* the controller compares bindings with where VMs run, every
  minute, and reports any port bound elsewhere (`mst_port_binding_mismatches`,
  the audit log's `network.port_binding_mismatch`): alert on it, and take
  that host out of service.
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
- **Backups are stored as they are.** A backup repository holds every
  backed-up VM's disks, unencrypted, for every tenant using it; NFS checks
  only the client's address, and the export needs `no_root_squash`.
  *Mitigation:* export repositories to the hosts' management addresses
  only, on a network guests and tenants cannot reach, and give tenants that
  must not share storage repositories of their own. Incremental backups'
  files name the files they build on; a restore follows only names of
  other backups' files in the same repository, so a tampered file cannot
  have a host file read into a restored disk.
- **Networking is IPv4 only.** Port security, security groups and
  guardrails are written for IPv4; guests' IPv6 is dropped (tested), not
  filtered.
- **Sealed storage pool keys are bound to "a storage pool", not to their
  pool.** Someone able to write to the database could swap two pools'
  sealed keys, and values written there unsealed are accepted (as they
  were before sealing existed).
  *Mitigation:* the database reachable by the controllers only, with
  credentials of their own.
- **QEMU before 9.0 (CVE-2024-4467)** can open files a crafted qcow2
  image names while `qemu-img info` reads it, before metastratum refuses
  the image.
  *Mitigation:* the supported distributions' current QEMU, which has the
  fix.
- **Inline styles are allowed** by the Content-Security-Policy (the serial
  terminal and the UI set some); inline scripts are not.

## Deployment checklist

What metastratum offers but leaves to the deployment to switch on.
`mst-controller doctor` checks most of it on a running installation and
says what to fix:

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
- Backup repositories exported to the hosts only.
- AppArmor or SELinux confining VMs on every host (the agent warns when
  libvirt confines none).
- An alert on `mst_port_binding_mismatches`.
- Fencing on every host of clusters with high availability.
- Per-VM limits on clusters.
- A second factor required at the provider tenant (Tenants → Sign-in), or
  sign-in through identity providers with multi-factor authentication;
  scoped, expiring API tokens for automation.
- `cargo audit` and `npm audit` with every update.
