// A minimal RFB 3.8 server (the VNC protocol, as QEMU speaks it to noVNC):
// no authentication, Raw rectangles in whatever pixel format the client
// asks for, key events passed on. What it shows is drawn on a canvas.
import type { Ctx } from './windows'

/** What the screen shows and does with keys. */
export interface Screen {
  draw(c: Ctx, width: number, height: number): void
  key(keysym: number, down: boolean): void
}

type PixelFormat = {
  bpp: number
  bigEndian: boolean
  rMax: number
  gMax: number
  bMax: number
  rShift: number
  gShift: number
  bShift: number
}

const DEFAULT_FORMAT: PixelFormat = { bpp: 32, bigEndian: false, rMax: 255, gMax: 255, bMax: 255, rShift: 16, gShift: 8, bShift: 0 }

function canvas(w: number, h: number): { ctx: Ctx } {
  if (typeof OffscreenCanvas !== 'undefined') {
    const c = new OffscreenCanvas(w, h)
    return { ctx: c.getContext('2d', { willReadFrequently: true })! }
  }
  const c = document.createElement('canvas')
  c.width = w
  c.height = h
  return { ctx: c.getContext('2d', { willReadFrequently: true })! }
}

export class RfbServer {
  private state: 'version' | 'security' | 'init' | 'normal' = 'version'
  private buf = new Uint8Array(0)
  private format = DEFAULT_FORMAT
  private readonly ctx: Ctx
  /** What the client was last sent, to send it only what changed. */
  private sent: Uint32Array | null = null
  private dirty = true
  private waiting = false

  constructor(
    private readonly send: (bytes: Uint8Array) => void,
    private readonly screen: Screen,
    private readonly name: string,
    readonly width = 1280,
    readonly height = 800,
  ) {
    this.ctx = canvas(width, height).ctx
  }

  start() {
    this.send(new TextEncoder().encode('RFB 003.008\n'))
  }

  /** The screen changed: the client gets it if it is waiting for it. */
  changed() {
    this.dirty = true
    if (this.waiting && this.state === 'normal') {
      this.waiting = false
      this.update(true, 0, 0, this.width, this.height)
    }
  }

  receive(bytes: Uint8Array) {
    const merged = new Uint8Array(this.buf.length + bytes.length)
    merged.set(this.buf)
    merged.set(bytes, this.buf.length)
    this.buf = merged
    for (;;) {
      const used = this.step()
      if (!used) break
      this.buf = this.buf.slice(used)
    }
  }

  /** Handles one message from the client; the bytes it took (0: wait). */
  private step(): number {
    const b = this.buf
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength)
    switch (this.state) {
      case 'version':
        if (b.length < 12) return 0
        this.state = 'security'
        // One security type: None.
        this.send(new Uint8Array([1, 1]))
        return 12
      case 'security':
        if (b.length < 1) return 0
        this.state = 'init'
        this.send(new Uint8Array([0, 0, 0, 0]))
        return 1
      case 'init': {
        if (b.length < 1) return 0
        this.state = 'normal'
        this.send(this.serverInit())
        return 1
      }
    }
    if (b.length < 1) return 0
    switch (b[0]) {
      case 0: // SetPixelFormat
        if (b.length < 20) return 0
        this.format = {
          bpp: b[4]!,
          bigEndian: b[6] !== 0,
          rMax: v.getUint16(8),
          gMax: v.getUint16(10),
          bMax: v.getUint16(12),
          rShift: b[14]!,
          gShift: b[15]!,
          bShift: b[16]!,
        }
        this.sent = null
        return 20
      case 2: {
        // SetEncodings: Raw is always fine.
        if (b.length < 4) return 0
        const n = v.getUint16(2)
        return b.length < 4 + 4 * n ? 0 : 4 + 4 * n
      }
      case 3: // FramebufferUpdateRequest
        if (b.length < 10) return 0
        this.request(b[1] !== 0, v.getUint16(2), v.getUint16(4), v.getUint16(6), v.getUint16(8))
        return 10
      case 4: // KeyEvent
        if (b.length < 8) return 0
        try {
          this.screen.key(v.getUint32(4), b[1] !== 0)
        } catch (e) {
          console.error('[demo console]', e)
        }
        return 8
      case 5: // PointerEvent
        return b.length < 6 ? 0 : 6
      case 6: {
        // ClientCutText (negative lengths: extended clipboard).
        if (b.length < 8) return 0
        const n = Math.abs(v.getInt32(4))
        return b.length < 8 + n ? 0 : 8 + n
      }
      default:
        // Something not asked for: what follows cannot be told apart.
        return b.length
    }
  }

  private serverInit(): Uint8Array {
    const name = new TextEncoder().encode(this.name)
    const out = new Uint8Array(24 + name.length)
    const v = new DataView(out.buffer)
    v.setUint16(0, this.width)
    v.setUint16(2, this.height)
    const f = DEFAULT_FORMAT
    out.set([f.bpp, 24, f.bigEndian ? 1 : 0, 1], 4)
    v.setUint16(8, f.rMax)
    v.setUint16(10, f.gMax)
    v.setUint16(12, f.bMax)
    out.set([f.rShift, f.gShift, f.bShift], 14)
    v.setUint32(20, name.length)
    out.set(name, 24)
    return out
  }

  private request(incremental: boolean, x: number, y: number, w: number, h: number) {
    if (incremental && !this.dirty) {
      this.waiting = true
      return
    }
    this.update(incremental, x, y, w, h)
  }

  private update(incremental: boolean, x: number, y: number, w: number, h: number) {
    this.dirty = false
    const W = this.width
    const H = this.height
    try {
      this.ctx.save()
      this.screen.draw(this.ctx, W, H)
      this.ctx.restore()
    } catch (e) {
      console.error('[demo console]', e)
    }
    const rgba = new Uint32Array(this.ctx.getImageData(0, 0, W, H).data.buffer)
    // What changed since the last update (all of what was asked, if not
    // incremental).
    let [x0, y0, x1, y1] = [Math.min(x, W), Math.min(y, H), Math.min(x + w, W), Math.min(y + h, H)]
    if (incremental && this.sent) {
      let minX = W
      let minY = H
      let maxX = -1
      let maxY = -1
      for (let row = 0; row < H; row++) {
        const o = row * W
        let first = -1
        let last = -1
        for (let col = 0; col < W; col++) {
          if (rgba[o + col] !== this.sent[o + col]) {
            if (first < 0) first = col
            last = col
          }
        }
        if (first >= 0) {
          minY = Math.min(minY, row)
          maxY = row
          minX = Math.min(minX, first)
          maxX = Math.max(maxX, last)
        }
      }
      if (maxX < 0) {
        // Nothing changed after all: wait for something that does.
        this.waiting = true
        return
      }
      ;[x0, y0, x1, y1] = [minX, minY, maxX + 1, maxY + 1]
    }
    this.sent = rgba.slice()
    const rw = x1 - x0
    const rh = y1 - y0
    if (rw <= 0 || rh <= 0) return
    const f = this.format
    const size = f.bpp / 8
    const out = new Uint8Array(4 + 12 + rw * rh * size)
    const v = new DataView(out.buffer)
    out[0] = 0 // FramebufferUpdate
    v.setUint16(2, 1)
    v.setUint16(4, x0)
    v.setUint16(6, y0)
    v.setUint16(8, rw)
    v.setUint16(10, rh)
    v.setInt32(12, 0) // Raw
    const fast = size === 4 && !f.bigEndian && f.rShift === 0 && f.gShift === 8 && f.bShift === 16 && f.rMax === 255
    let p = 16
    const bytes = new Uint8Array(rgba.buffer)
    for (let row = y0; row < y1; row++) {
      if (fast) {
        out.set(bytes.subarray((row * W + x0) * 4, (row * W + x1) * 4), p)
        p += rw * 4
        continue
      }
      for (let col = x0; col < x1; col++) {
        const i = (row * W + col) * 4
        const px =
          (Math.round((bytes[i]! * f.rMax) / 255) << f.rShift) |
          (Math.round((bytes[i + 1]! * f.gMax) / 255) << f.gShift) |
          (Math.round((bytes[i + 2]! * f.bMax) / 255) << f.bShift)
        for (let k = 0; k < size; k++) {
          const shift = f.bigEndian ? (size - 1 - k) * 8 : k * 8
          out[p++] = (px >>> shift) & 0xff
        }
      }
    }
    this.send(out)
  }
}

/** A text console (a VGA tty): a grid of characters a terminal's output is written to. */
export class TextScreen implements Screen {
  private readonly cols: number
  private readonly rows: number
  private grid: string[][]
  private x = 0
  private y = 0
  private esc = ''
  private static readonly CW = 10
  private static readonly CH = 20

  constructor(
    width: number,
    height: number,
    private readonly onKey: (sym: number, down: boolean) => void,
  ) {
    this.cols = Math.floor(width / TextScreen.CW)
    this.rows = Math.floor(height / TextScreen.CH)
    this.grid = this.blank()
  }

  private blank() {
    return Array.from({ length: this.rows }, () => Array<string>(this.cols).fill(' '))
  }

  key(sym: number, down: boolean) {
    this.onKey(sym, down)
  }

  /** Terminal output: text with \r, \n, \b and a few escape sequences. */
  write(s: string) {
    for (const ch of s) {
      if (this.esc) {
        this.esc += ch
        if (this.esc.length === 2 && ch !== '[') this.esc = ''
        else if (this.esc.length > 2 && /[@-~]/.test(ch)) {
          this.csi(this.esc)
          this.esc = ''
        }
        continue
      }
      if (ch === '\x1b') this.esc = ch
      else if (ch === '\r') this.x = 0
      else if (ch === '\n') this.newline()
      else if (ch === '\b') this.x = Math.max(0, this.x - 1)
      else if (ch >= ' ') {
        if (this.x >= this.cols) {
          this.x = 0
          this.newline()
        }
        this.grid[this.y]![this.x++] = ch
      }
    }
  }

  private csi(seq: string) {
    const end = seq.at(-1)
    if (end === 'J') this.grid = this.blank()
    else if (end === 'H') [this.x, this.y] = [0, 0]
    else if (end === 'K') for (let i = this.x; i < this.cols; i++) this.grid[this.y]![i] = ' '
  }

  private newline() {
    if (this.y < this.rows - 1) this.y++
    else {
      this.grid.shift()
      this.grid.push(Array<string>(this.cols).fill(' '))
    }
  }

  draw(c: Ctx, w: number, h: number) {
    c.fillStyle = '#000'
    c.fillRect(0, 0, w, h)
    c.font = `16px 'Martian Mono', 'DejaVu Sans Mono', ui-monospace, monospace`
    c.textBaseline = 'top'
    c.fillStyle = '#aaaaaa'
    for (let r = 0; r < this.rows; r++) {
      const line = this.grid[r]!
      for (let col = 0; col < this.cols; col++) {
        const ch = line[col]!
        if (ch !== ' ') c.fillText(ch, col * TextScreen.CW, r * TextScreen.CH + 2)
      }
    }
    // The cursor: an underline, as the VGA console's.
    c.fillRect(this.x * TextScreen.CW, this.y * TextScreen.CH + TextScreen.CH - 4, TextScreen.CW, 2)
  }
}
