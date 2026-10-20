// IDs and times as the controller makes them (no imports: the seed and the
// store both use these, and the store loads the seed as it loads).

/** A new ID: a UUIDv7, as the controller makes them (time first). */
export function newId(): string {
  const b = crypto.getRandomValues(new Uint8Array(16))
  let t = Date.now()
  for (let i = 5; i >= 0; i--) {
    b[i] = t & 0xff
    t = Math.floor(t / 256)
  }
  b[6] = (b[6]! & 0x0f) | 0x70
  b[8] = (b[8]! & 0x3f) | 0x80
  const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('')
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}

export const now = () => new Date().toISOString()
