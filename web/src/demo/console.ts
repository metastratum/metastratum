// The VMs' consoles in the demo. A console ticket's WebSocket
// (`/api/v1/console/{ticket}`) is answered in the page: the serial console
// by a scripted guest (a Linux login and shell, or Windows' SAC), the
// graphical one by a small VNC server drawing a text console or Windows'
// lock screen. Both close, as the controller's do, once the VM stops.
import { keysymChar, SacConsole, WindowsScreen } from './console/windows'
import { LinuxConsole } from './console/linux'
import { RfbServer, TextScreen } from './console/rfb'
import { vmForTicket } from './handlers/vms'
import { db, type VmRec } from './store'

export function isConsoleUrl(url: string): boolean {
  return url.includes('/api/v1/console/')
}

/** What runs behind a console socket. */
interface Session {
  receive(bytes: Uint8Array): void
  stop(): void
}

type Handler<E extends Event> = ((ev: E) => void) | null

/**
 * Stands in for the browser's WebSocket on a console URL. Everything a
 * client uses lives on the instance or this class's own prototype (noVNC
 * checks for them there).
 */
export class ConsoleSocket extends EventTarget {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSING = 2
  static readonly CLOSED = 3
  readonly CONNECTING = 0
  readonly OPEN = 1
  readonly CLOSING = 2
  readonly CLOSED = 3
  readonly url: string
  protocol = ''
  extensions = ''
  binaryType: BinaryType = 'blob'
  readyState = 0
  bufferedAmount = 0
  onopen: Handler<Event> = null
  onmessage: Handler<MessageEvent> = null
  onclose: Handler<CloseEvent> = null
  onerror: Handler<Event> = null
  private session: Session | null = null
  private watch = 0

  constructor(url: string) {
    super()
    this.url = url
    setTimeout(() => this.connect(), 30)
  }

  send(data: string | ArrayBufferLike | ArrayBufferView | Blob) {
    if (this.readyState === 0) throw new DOMException('Still in CONNECTING state.', 'InvalidStateError')
    if (this.readyState !== 1) return
    const deliver = (bytes: Uint8Array) =>
      setTimeout(() => {
        try {
          this.session?.receive(bytes)
        } catch (e) {
          console.error('[demo console]', e)
        }
      }, 0)
    // Copied now: senders reuse their buffers.
    if (typeof data === 'string') deliver(new TextEncoder().encode(data))
    else if (data instanceof Blob) void data.arrayBuffer().then((b) => deliver(new Uint8Array(b)))
    else if (ArrayBuffer.isView(data)) deliver(new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice())
    else deliver(new Uint8Array(data as ArrayBuffer).slice())
  }

  close(code = 1000, reason = '') {
    if (this.readyState >= 2) return
    this.readyState = 2
    this.finish(code, reason)
  }

  private fire<E extends Event>(ev: E, handler: Handler<E>) {
    try {
      handler?.call(this, ev)
    } catch (e) {
      console.error('[demo console]', e)
    }
    this.dispatchEvent(ev)
  }

  private finish(code: number, reason: string) {
    clearInterval(this.watch)
    this.session?.stop()
    this.session = null
    setTimeout(() => {
      this.readyState = 3
      this.fire(new CloseEvent('close', { code, reason, wasClean: code !== 1006 }), this.onclose)
    }, 0)
  }

  /** Data from the console to the client. */
  private deliver(bytes: Uint8Array) {
    if (this.readyState !== 1) return
    const data = this.binaryType === 'arraybuffer' ? bytes.slice().buffer : new Blob([bytes.slice()])
    this.fire(new MessageEvent('message', { data }), this.onmessage)
  }

  private connect() {
    if (this.readyState !== 0) return
    const ticket = decodeURIComponent(this.url.split('/console/')[1]?.split(/[?#]/)[0] ?? '')
    let found: ReturnType<typeof vmForTicket>
    try {
      found = vmForTicket(ticket)
    } catch {
      found = undefined
    }
    if (!found) {
      // The controller refuses the upgrade (401): the socket fails.
      this.readyState = 3
      this.fire(new Event('error'), this.onerror)
      this.fire(new CloseEvent('close', { code: 1006, reason: '', wasClean: false }), this.onclose)
      return
    }
    const { vm, kind } = found
    this.protocol = kind === 'vnc' ? 'binary' : ''
    this.readyState = 1
    this.fire(new Event('open'), this.onopen)
    if (!running(vm.id)) return this.close(1011, 'the VM is not running')
    try {
      this.session = kind === 'vnc' ? graphical(vm, (b) => this.deliver(b)) : serial(vm, (b) => this.deliver(b))
    } catch (e) {
      console.error('[demo console]', e)
      return this.close(1011, 'the console failed')
    }
    // Gone once the VM stops (or goes).
    this.watch = window.setInterval(() => {
      if (!running(vm.id)) this.close(1011, 'the VM is not running')
    }, 1500)
  }
}

function running(id: string): boolean {
  return db.vms.find((v) => v.id === id)?.power === 'running'
}

/** Output gathered within a tick and sent as one message. */
function batched(send: (bytes: Uint8Array) => void) {
  let pending = ''
  return (s: string) => {
    if (!pending) {
      setTimeout(() => {
        const out = pending
        pending = ''
        send(new TextEncoder().encode(out))
      }, 0)
    }
    pending += s
  }
}

function serial(vm: VmRec, send: (bytes: Uint8Array) => void): Session {
  const out = batched(send)
  const guest = vm.guest_os === 'windows' ? new SacConsole(vm, out) : new LinuxConsole(vm, out, 'ttyS0')
  const decoder = new TextDecoder()
  return {
    receive: (bytes) => guest.input(decoder.decode(bytes, { stream: true })),
    stop: () => {},
  }
}

function graphical(vm: VmRec, send: (bytes: Uint8Array) => void): Session {
  const W = 1280
  const H = 800
  let server: RfbServer
  let clock = 0
  if (vm.guest_os === 'windows') {
    const screen = new WindowsScreen(vm, () => server.changed())
    server = new RfbServer(send, screen, vm.name, W, H)
    // The clock on the lock screen and taskbar.
    clock = window.setInterval(() => server.changed(), 30_000)
  } else {
    let ctrl = false
    const tty = new TextScreen(W, H, (sym, down) => {
      if (sym === 0xffe3 || sym === 0xffe4) ctrl = down
      if (!down) return
      const ch = keyInput(sym, ctrl)
      if (ch) shell.input(ch)
    })
    const shell = new LinuxConsole(
      vm,
      (s) => {
        tty.write(s)
        server.changed()
      },
      'tty1',
    )
    server = new RfbServer(send, tty, vm.name, W, H)
    shell.greet()
  }
  server.start()
  return {
    receive: (bytes) => server.receive(bytes),
    stop: () => clearInterval(clock),
  }
}

/** What a key types into a terminal (as a terminal sends it). */
function keyInput(sym: number, ctrl: boolean): string | null {
  switch (sym) {
    case 0xff08:
      return '\x7f'
    case 0xff09:
      return '\t'
    case 0xff0d:
    case 0xff8d:
      return '\r'
    case 0xff1b:
      return '\x1b'
    case 0xff52:
      return '\x1b[A'
    case 0xff54:
      return '\x1b[B'
  }
  const ch = keysymChar(sym)
  if (!ch) return null
  if (ctrl && /^[a-z]$/i.test(ch)) return String.fromCharCode(ch.toLowerCase().charCodeAt(0) - 96)
  return ch
}
