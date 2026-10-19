# Two-factor sign-in

People with a password can add second factors on their account page, and
signing in then takes one of them as well:

- **Authenticator apps** (TOTP: Google Authenticator, 1Password, Aegis…),
  from a QR code.
- **Security keys and passkeys** over WebAuthn: FIDO2 keys, older FIDO U2F
  keys, and platform passkeys. A key works at the address it was added at
  (`mst.example.com`), since browsers bind it there; browsers use keys only
  at a host name over HTTPS, or at `localhost`.
- **YubiKey OTPs**: the one-time passwords a YubiKey types when touched,
  where the controllers can reach a Yubico validation service. Give them
  its client ID and API key (the `[yubico]` section; YubiCloud by
  default, or `servers` for your own). A YubiKey also works as a security
  key, which needs no service.
- **Backup codes**: ten single-use codes, shown once when the first factor
  is added and replaceable from the account page.

A tenant's administrators can require a second factor of its password
accounts and those of its sub-tenants (Tenants → Sign-in); a sub-tenant
cannot turn off what a tenant above it requires. Someone without one can,
once signed in, only add one. People who sign in through an identity
provider use its second factors instead, and API tokens sign in alone.

Someone who lost theirs is reset by an administrator of their tenant
(Tenants → People → Reset second factor), which signs them out; they then
sign in with their password and add new ones. Where someone else may have
got into the account, revoke its API tokens with the reset (or with a new
password); a plain reset leaves them, so the person's scripts keep
working. For the last provider
administrator, on a controller:

```sh
mst-controller reset-two-factor admin
mst-controller reset-two-factor --tenant acme/dev alice
```
