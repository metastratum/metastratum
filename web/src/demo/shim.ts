// Sends the API's requests to the demo backend. Imports nothing, so it is in
// place early (bundled chunks need not run in import order); a request made
// before the backend is plugged in waits for it.
let backend: ((req: Request) => Promise<Response>) | null = null

/** Where the API's requests go from now on. */
export function plug(fn: (req: Request) => Promise<Response>) {
  backend = fn
}

const realFetch = window.fetch.bind(window)
window.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const req = new Request(input, init)
  const url = new URL(req.url, location.href)
  if (url.origin !== location.origin || !url.pathname.startsWith('/api/')) return realFetch(input, init)
  // Not before the backend is there (it is, by the time the app asks).
  while (!backend) await new Promise((res) => setTimeout(res, 10))
  return backend(req)
}
