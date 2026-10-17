// Security keys through WebAuthn: the controller sends options and takes
// answers in WebAuthn's JSON form (binary fields as base64url); the browser
// API wants and gives ArrayBuffers.

const toBuffer = (s: string): ArrayBuffer => {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/')
  const bin = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4))
  return Uint8Array.from(bin, (c) => c.charCodeAt(0)).buffer
}

const fromBuffer = (b: ArrayBuffer): string => {
  let bin = ''
  for (const byte of new Uint8Array(b)) bin += String.fromCharCode(byte)
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

type Descriptor = { id: string; type: string; transports?: string[] }
type Json = Record<string, unknown>

const descriptors = (list: unknown) =>
  ((list as Descriptor[] | undefined) ?? []).map((d) => ({
    ...d,
    type: 'public-key' as const,
    id: toBuffer(d.id),
    transports: d.transports as AuthenticatorTransport[] | undefined,
  }))

/** Whether this browser, at this address, can use security keys. Browsers
 *  refuse them at IP addresses and, except at localhost, without HTTPS. */
export function securityKeysWorkHere(): boolean {
  const host = location.hostname
  const ip = /^[\d.]+$/.test(host) || host.startsWith('[') || host.includes(':')
  return 'PublicKeyCredential' in window && window.isSecureContext && !ip
}

/** Registers a key: `navigator.credentials.create()` with the controller's
 *  options, answered in JSON. */
export async function createCredential(options: Json): Promise<Json> {
  const o = options as Json & { user: Json; challenge: string }
  const cred = (await navigator.credentials.create({
    publicKey: {
      ...(o as unknown as PublicKeyCredentialCreationOptions),
      challenge: toBuffer(o.challenge),
      user: { ...(o.user as unknown as PublicKeyCredentialUserEntity), id: toBuffer(o.user.id as string) },
      excludeCredentials: descriptors(o.excludeCredentials),
    },
  })) as PublicKeyCredential | null
  if (!cred) throw new Error('No key answered.')
  const r = cred.response as AuthenticatorAttestationResponse
  return {
    id: cred.id,
    rawId: fromBuffer(cred.rawId),
    type: cred.type,
    response: {
      clientDataJSON: fromBuffer(r.clientDataJSON),
      attestationObject: fromBuffer(r.attestationObject),
      transports: r.getTransports?.() ?? [],
    },
  }
}

/** Signs in with a key: `navigator.credentials.get()`, answered in JSON. */
export async function getAssertion(options: Json, signal?: AbortSignal): Promise<Json> {
  const o = options as Json & { challenge: string }
  const cred = (await navigator.credentials.get({
    signal,
    publicKey: {
      ...(o as unknown as PublicKeyCredentialRequestOptions),
      challenge: toBuffer(o.challenge),
      allowCredentials: descriptors(o.allowCredentials),
    },
  })) as PublicKeyCredential | null
  if (!cred) throw new Error('No key answered.')
  const r = cred.response as AuthenticatorAssertionResponse
  return {
    id: cred.id,
    rawId: fromBuffer(cred.rawId),
    type: cred.type,
    response: {
      clientDataJSON: fromBuffer(r.clientDataJSON),
      authenticatorData: fromBuffer(r.authenticatorData),
      signature: fromBuffer(r.signature),
    },
  }
}

/** What to say when the browser gave up: the user cancelled or timed out,
 *  or the key was already registered. */
export function keyErrorText(e: unknown): string {
  if (e instanceof DOMException) {
    if (e.name === 'NotAllowedError') return 'The key didn’t answer: it was cancelled or timed out.'
    if (e.name === 'InvalidStateError') return 'That key is already registered.'
    if (e.name === 'SecurityError') return 'This browser won’t use security keys at this address.'
  }
  return e instanceof Error ? e.message : 'The key didn’t answer.'
}
