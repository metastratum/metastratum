import { describe, expect, it } from 'vitest'
import { db as data } from '../store'
import { LinuxConsole } from './linux'
import { RfbServer, TextScreen } from './rfb'
import { SacConsole } from './windows'

const linuxVm = data.vms.find((v) => v.guest_os === 'linux' && v.power === 'running')!
const windowsVm = data.vms.find((v) => v.guest_os === 'windows')!

function session<T extends { input(s: string): void }>(make: (out: (s: string) => void) => T) {
  let screen = ''
  const guest = make((s) => (screen += s))
  return {
    type: (s: string) => guest.input(s),
    take: () => {
      const s = screen
      screen = ''
      return s
    },
  }
}

describe('demo consoles', () => {
  it('logs in to a Linux guest and answers from its record', () => {
    const t = session((out) => new LinuxConsole(linuxVm, out, 'ttyS0'))
    t.type('\r')
    expect(t.take()).toContain(`${linuxVm.name} login: `)
    t.type('debian\r')
    expect(t.take()).toContain('Password: ')
    t.type('secret\r')
    expect(t.take()).toContain(`debian@${linuxVm.name}:~$ `)
    t.type('ip a\r')
    const ip = t.take()
    expect(ip).toContain(linuxVm.nics[0]!.ip)
    expect(ip).toContain(linuxVm.nics[0]!.mac)
    t.type('sudo -i\r')
    expect(t.take()).toContain(`root@${linuxVm.name}:~# `)
    t.type('exit\r')
    expect(t.take()).toContain(`debian@${linuxVm.name}:~$ `)
    t.type('frob\r')
    expect(t.take()).toContain('frob: command not found')
    t.type('exit\r')
    expect(t.take()).toContain('login: ')
  })

  it("opens a command prompt through Windows' SAC", () => {
    const t = session((out) => new SacConsole(windowsVm, out))
    t.type('\r')
    expect(t.take()).toContain('SAC>')
    t.type('cmd\r')
    expect(t.take()).toContain('Channel: Cmd0001')
    t.type('ch -si 1\r')
    t.type(' ')
    expect(t.take()).toContain('Username: ')
    t.type('admin\r\rpw\r')
    expect(t.take()).toContain('C:\\Windows\\system32>')
    t.type('hostname\r')
    expect(t.take()).toContain(windowsVm.name.slice(0, 15).toUpperCase())
  })

  it('speaks RFB: handshake, a full update, then only what changed', () => {
    const W = 64
    const H = 40
    // A canvas that is just pixels: what is drawn is the text grid's count.
    let frame = 0
    const ctx = new Proxy({} as Record<string, unknown>, {
      get: (_, k) =>
        k === 'getImageData'
          ? () => {
              const px = new Uint8ClampedArray(W * H * 4)
              px[0] = frame
              return { data: px }
            }
          : () => ({ addColorStop() {} }),
      set: () => true,
    })
    ;(globalThis as Record<string, unknown>).OffscreenCanvas = class {
      getContext() {
        return ctx
      }
    }
    const sent: Uint8Array[] = []
    const screen = new TextScreen(W, H, () => {
      frame++
      server.changed()
    })
    const server = new RfbServer((b) => sent.push(b), screen, 'vm', W, H)
    server.start()
    expect(new TextDecoder().decode(sent[0])).toBe('RFB 003.008\n')
    server.receive(new TextEncoder().encode('RFB 003.008\n'))
    expect([...sent[1]!]).toEqual([1, 1])
    server.receive(new Uint8Array([1]))
    expect([...sent[2]!]).toEqual([0, 0, 0, 0])
    server.receive(new Uint8Array([1]))
    const init = new DataView(sent[3]!.buffer)
    expect([init.getUint16(0), init.getUint16(2)]).toEqual([W, H])
    // noVNC's pixel format (RGBX), encodings, and a full update request.
    const pf = new Uint8Array(20)
    pf.set([0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16])
    server.receive(pf)
    server.receive(new Uint8Array([2, 0, 0, 1, 0, 0, 0, 0]))
    server.receive(new Uint8Array([3, 0, 0, 0, 0, 0, 0, W, 0, H]))
    expect(sent[4]!.length).toBe(16 + W * H * 4)
    // Nothing changed: an incremental request waits; a key answers it.
    server.receive(new Uint8Array([3, 1, 0, 0, 0, 0, 0, W, 0, H]))
    expect(sent.length).toBe(5)
    server.receive(new Uint8Array([4, 1, 0, 0, 0, 0, 0, 0x61]))
    expect(sent.length).toBe(6)
    const rect = new DataView(sent[5]!.buffer)
    expect([rect.getUint16(8), rect.getUint16(10)]).toEqual([1, 1])
  })
})
