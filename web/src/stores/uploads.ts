import { reactive } from 'vue'
import { ApiError, type Image } from '../api/client'

/** What one request carries of a file (the API takes up to 32 MiB). */
const PART = 16 << 20

/** Uploads under way in this tab, by image ID. */
export const uploads = reactive(
  new Map<string, { sent: number; total: number; abort: AbortController; error: string }>(),
)

// Leaving the page stops them: ask first.
window.addEventListener('beforeunload', (e) => {
  if ([...uploads.values()].some((u) => !u.error)) e.preventDefault()
})

/** Whether the file is qcow2 (by its first bytes), or else raw. */
export async function detectFormat(file: File): Promise<'qcow2' | 'raw'> {
  const head = new Uint8Array(await file.slice(0, 4).arrayBuffer())
  return head[0] === 0x51 && head[1] === 0x46 && head[2] === 0x49 && head[3] === 0xfb ? 'qcow2' : 'raw'
}

async function failure(res: Response): Promise<ApiError> {
  const e = (await res.json().catch(() => ({}))) as { error?: string; message?: string }
  return new ApiError(res.status, e.error ?? 'error', e.message ?? `Request failed (${res.status})`)
}

/**
 * Uploads `file` to an image added with its size, from `from` (what has
 * arrived), in parts. A part the server did not take (it says from where to
 * go on) or a dropped connection is sent again; it gives up after five
 * failures in a row.
 */
export async function upload(image: Image, file: File, from: number, tenant: string): Promise<void> {
  const total = file.size
  const abort = new AbortController()
  const entry = reactive({ sent: from, total, abort, error: '' })
  uploads.set(image.id, entry)
  let failures = 0
  try {
    while (entry.sent < total) {
      const end = Math.min(entry.sent + PART, total)
      let res: Response
      try {
        res = await fetch(`/api/v1/images/${image.id}/upload?offset=${entry.sent}`, {
          method: 'PUT',
          credentials: 'same-origin',
          headers: { 'content-type': 'application/octet-stream' },
          body: file.slice(entry.sent, end),
          signal: abort.signal,
        })
      } catch (e) {
        if (abort.signal.aborted) throw e
        if (++failures >= 5) throw new ApiError(0, 'network', 'The connection keeps dropping. Resume the upload later.')
        await new Promise((r) => setTimeout(r, 2000 * failures))
        continue
      }
      if (res.ok) {
        entry.sent = end
        failures = 0
        continue
      }
      // Not the next part (one was lost on the way): go on from what arrived.
      if (res.status === 409) {
        const now = await arrivedIn(image.id, tenant)
        if (now.state !== 'uploading' || ++failures >= 5) throw await failure(res)
        entry.sent = now.uploaded_bytes ?? 0
        continue
      }
      throw await failure(res)
    }
    uploads.delete(image.id)
  } catch (e) {
    entry.error = abort.signal.aborted
      ? 'Upload stopped.'
      : e instanceof ApiError
        ? e.message
        : 'The upload failed. Resume it later.'
  }
}

async function arrivedIn(id: string, tenant: string): Promise<Image> {
  const res = await fetch(`/api/v1/images?tenant_id=${tenant}`, { credentials: 'same-origin' })
  if (!res.ok) throw await failure(res)
  const img = ((await res.json()) as Image[]).find((i) => i.id === id)
  if (!img) throw new ApiError(404, 'not_found', 'The image is gone.')
  return img
}

/** Stops an upload (it can be resumed). */
export function stopUpload(id: string) {
  uploads.get(id)?.abort.abort()
}

/** Forgets a stopped or failed upload's note. */
export function dismissUpload(id: string) {
  uploads.delete(id)
}

