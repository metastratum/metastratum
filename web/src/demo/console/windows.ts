// A Windows guest's consoles: the Special Administration Console (SAC) on
// its serial port, with a command prompt channel to sign in to; and on its
// screen, the lock screen and a plain desktop behind it.
import { db, type VmRec } from '../store'
import { computerName, interfaces, linkLocal, memoryMib, uptimeSeconds, windowsEdition } from './guest'

type Out = (s: string) => void
const NL = '\r\n'

type Mode = 'sac' | 'switch' | 'username' | 'domain' | 'password' | 'cmd'

export class SacConsole {
  private mode: Mode = 'sac'
  private line = ''
  private channel = false
  private user = ''
  private esc = false
  private greeted = false

  constructor(
    private readonly vm: VmRec,
    private readonly out: Out,
  ) {}

  private get account() {
    return this.vm.account ?? 'Administrator'
  }

  greet() {
    this.greeted = true
    this.out(
      [
        '',
        'Computer is booting, SAC started and initialized.',
        '',
        'Use the "ch -?" command for information about using channels.',
        'Use the "?" command for general help.',
        '',
        '',
        'SAC>',
      ].join(NL),
    )
  }

  input(data: string) {
    for (const ch of data) {
      try {
        this.key(ch)
      } catch (e) {
        console.error('[demo console]', e)
      }
    }
  }

  private key(ch: string) {
    if (!this.greeted) {
      this.greet()
      if (ch === '\r' || ch === '\n') return
    }
    // <esc><tab> goes back to SAC from a channel.
    if (this.esc) {
      this.esc = false
      if (ch === '\t' && this.mode === 'cmd') {
        this.mode = 'sac'
        this.line = ''
        this.out(`${NL}SAC>`)
      }
      return
    }
    if (ch === '\x1b') {
      this.esc = true
      return
    }
    if (this.mode === 'switch') {
      this.mode = 'username'
      this.out(`\x1b[2J\x1b[HPlease enter login credentials.${NL}Username: `)
      return
    }
    if (ch === '\r' || ch === '\n') return this.enter()
    if (ch === '\x7f' || ch === '\b') {
      if (this.line) {
        this.line = this.line.slice(0, -1)
        if (this.mode !== 'password') this.out('\b \b')
      }
      return
    }
    if (ch === '\x03') {
      this.line = ''
      this.out(`^C${NL}`)
      return this.prompt()
    }
    if (ch < ' ') return
    this.line += ch
    if (this.mode !== 'password') this.out(ch)
  }

  private prompt() {
    if (this.mode === 'sac') this.out('SAC>')
    else if (this.mode === 'cmd') this.out('C:\\Windows\\system32>')
  }

  private enter() {
    const line = this.line.trim()
    this.line = ''
    this.out(NL)
    switch (this.mode) {
      case 'username':
        this.user = line
        this.mode = 'domain'
        return this.out('Domain  : ')
      case 'domain':
        this.mode = 'password'
        return this.out('Password: ')
      case 'password':
        if (!this.user) {
          this.mode = 'username'
          return this.out(`The username or password is incorrect.${NL}Username: `)
        }
        this.mode = 'cmd'
        this.out(`\x1b[2J\x1b[HMicrosoft Windows [Version 10.0.26100.6584]${NL}(c) Microsoft Corporation. All rights reserved.${NL}${NL}`)
        return this.prompt()
      case 'cmd':
        this.cmd(line)
        return this.mode === 'cmd' ? this.prompt() : undefined
      default:
        this.sac(line)
        return this.prompt()
    }
  }

  private print(lines: string[]) {
    this.out(lines.join(NL) + NL)
  }

  private sac(line: string) {
    const [cmd = '', ...args] = line.split(/\s+/)
    switch (cmd.toLowerCase()) {
      case '':
        return
      case '?':
      case 'help':
        return this.print([
          'ch                   Channel management commands.  Use ch -? for more help.',
          'cmd                  Create a Command Prompt channel.',
          'd                    Dump the current kernel log.',
          'f                    Toggle detailed or abbreviated tlist info.',
          '? or help            Display this list.',
          'i                    List all IP network numbers and their IP addresses.',
          'id                   Display the computer identification information.',
          'k <pid>              Kill the given process.',
          'r <pid>              Raise the priority of a process by one.',
          'restart              Restart the system immediately.',
          's                    Display the current time and date (24 hour clock used).',
          'shutdown             Shutdown the system immediately.',
          't                    Tlist.',
        ])
      case 'i':
        return this.print(
          interfaces(this.vm, false).map(
            (i, n) => `Net: ${n + 10}, Ip=${i.ip}  Subnet=${maskOf(i.prefix)}  Gateway=${i.gateway}`,
          ),
        )
      case 'id':
        return this.print([
          `Computer Name: ${computerName(this.vm)}`,
          `Computer GUID: ${this.vm.id}`,
          'Processor Architecture: x64',
          `Build Number: 26100`,
          `Product: ${windowsEdition(this.vm)}`,
          'Applied Service Pack: None',
          `Last Shutdown Time: ${new Date(Date.now() - uptimeSeconds(this.vm) * 1000).toLocaleString('en-US')}`,
        ])
      case 's':
        return this.print([new Date().toLocaleString('en-US', { hour12: false })])
      case 't':
        return this.print([
          'Time      PID   PPID  Thds  Image',
          '0:00:09.4   4      0   180  System',
          '0:00:00.2 432      4     4  smss.exe',
          '0:00:01.1 604    588    13  csrss.exe',
          '0:00:00.4 712    588     5  wininit.exe',
          '0:00:02.6 848    712    11  services.exe',
          '0:00:00.8 868    712    10  lsass.exe',
          '0:00:01.3 1824   848     9  qemu-ga.exe',
          '0:00:00.6 2016   848     7  cloudbase-init.exe',
        ])
      case 'restart':
      case 'shutdown':
        return this.print(["In the demo, use the VM page's power actions."])
      case 'cmd':
        this.channel = true
        return this.print([
          'The Command Prompt session was successfully launched.',
          'SAC>',
          'EVENT:   A new channel has been created.  Use "ch -?" for channel help.',
          'Channel: Cmd0001',
        ])
      case 'ch':
        if (!args.length) {
          return this.print([
            'Channel List',
            '',
            '(Use "ch -?" for information on using channels)',
            '',
            '# Status  Channel Name',
            '0 (AV)    SAC',
            ...(this.channel ? ['1 (AV)    Cmd0001'] : []),
          ])
        }
        if (args[0] === '-?') {
          return this.print([
            'Channel management commands:',
            'ch                   List all channels.',
            'ch -si <#>           Switch to a channel by its number.',
            'ch -sn <name>        Switch to a channel by its name.',
            'ch -ci <#>           Close a channel by its number.',
            'Press <esc><tab> to return to SAC.',
          ])
        }
        if ((args[0] === '-si' && args[1] === '1') || (args[0] === '-sn' && args[1]?.toLowerCase() === 'cmd0001')) {
          if (!this.channel) return this.print(['Error: There is no channel present at the specified index.'])
          this.mode = 'switch'
          this.out(
            [
              'Name:                  Cmd0001',
              'Description:           Command',
              'Type:                  VT-UTF8',
              'Channel GUID:          ' + this.vm.id,
              'Application Type GUID: 63d02271-8aa4-11d5-bccf-00b0d014a2d0',
              '',
              'Press <esc><tab> for next channel.',
              'Press <esc><tab>0 to return to the SAC channel.',
              'Use any other key to view this channel.',
              '',
            ].join(NL),
          )
          return
        }
        if (args[0] === '-ci' && args[1] === '1') {
          this.channel = false
          return this.print(['The channel was closed.'])
        }
        return this.print(['Error: Invalid channel command.'])
      default:
        return this.print(['Unrecognized command.  Try the \'help\' command for more details.'])
    }
  }

  private cmd(line: string) {
    const [cmd = '', ...args] = line.split(/\s+/)
    const name = computerName(this.vm)
    const p = (lines: string[]) => this.print([...lines, ''])
    switch (cmd.toLowerCase()) {
      case '':
        return
      case 'exit':
        this.channel = false
        this.mode = 'sac'
        this.out(`${NL}SAC>`)
        return
      case 'cls':
        this.out('\x1b[2J\x1b[H')
        return
      case 'whoami':
        return p([`${name.toLowerCase()}\\${(this.user || this.account).toLowerCase()}`])
      case 'hostname':
        return p([name])
      case 'ver':
        return p(['', 'Microsoft Windows [Version 10.0.26100.6584]'])
      case 'echo':
        return p([args.join(' ').replace(/%COMPUTERNAME%/gi, name).replace(/%USERNAME%/gi, this.user || this.account)])
      case 'ipconfig': {
        const out = ['', 'Windows IP Configuration', '']
        for (const i of interfaces(this.vm, false)) {
          out.push(
            `Ethernet adapter ${i.name}:`,
            '',
            '   Connection-specific DNS Suffix  . : ',
            `   Link-local IPv6 Address . . . . . : ${linkLocal(i.mac)}%${4 + out.length}`,
            `   IPv4 Address. . . . . . . . . . . : ${i.ip}`,
            `   Subnet Mask . . . . . . . . . . . : ${maskOf(i.prefix)}`,
            `   Default Gateway . . . . . . . . . : ${i.gateway}`,
            '',
          )
        }
        return p(out)
      }
      case 'systeminfo': {
        const m = memoryMib(this.vm)
        const vm = db.vms.find((v) => v.id === this.vm.id) ?? this.vm
        return p([
          '',
          `Host Name:                 ${name}`,
          `OS Name:                   Microsoft ${windowsEdition(vm)}`,
          'OS Version:                10.0.26100 N/A Build 26100',
          'System Manufacturer:       QEMU',
          'System Model:              Standard PC (Q35 + ICH9, 2009)',
          'System Type:               x64-based PC',
          `Processor(s):              ${vm.vcpus} Processor(s) Installed.`,
          `Total Physical Memory:     ${m.total.toLocaleString('en-US')} MB`,
          `Available Physical Memory: ${m.available.toLocaleString('en-US')} MB`,
          `Time Zone:                 (UTC) Coordinated Universal Time`,
          'Hyper-V Requirements:      A hypervisor has been detected.',
        ])
      }
      case 'dir':
        return p([
          ' Volume in drive C has no label.',
          ` Volume Serial Number is ${this.vm.id.slice(0, 4).toUpperCase()}-${this.vm.id.slice(4, 8).toUpperCase()}`,
          '',
          ' Directory of C:\\Windows\\system32',
          '',
          '10/14/2026  09:12 AM    <DIR>          .',
          '10/14/2026  09:12 AM    <DIR>          ..',
          '10/14/2026  09:10 AM           289,792 cmd.exe',
          '10/14/2026  09:10 AM           507,904 notepad.exe',
          '10/14/2026  09:10 AM           455,168 powershell.exe',
          '               3 File(s)      1,252,864 bytes',
          '               2 Dir(s)  41,203,736,576 bytes free',
        ])
      case 'shutdown':
        return p(["In the demo, use the VM page's power actions."])
      case 'help':
        return p(['Try: whoami  hostname  ver  ipconfig  systeminfo  dir  cls  exit', 'Press <esc><tab> to return to SAC.'])
      default:
        return p([`'${cmd}' is not recognized as an internal or external command,`, 'operable program or batch file.'])
    }
  }
}

function maskOf(prefix: number): string {
  const m = prefix ? (0xffffffff << (32 - prefix)) >>> 0 : 0
  return [24, 16, 8, 0].map((s) => (m >>> s) & 255).join('.')
}

/** A 2D context: an OffscreenCanvas's or a canvas's. */
export type Ctx = OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D

/** The Windows lock screen, and a plain desktop once signed in. */
export class WindowsScreen {
  private state: 'lock' | 'password' | 'welcome' | 'desktop' = 'lock'
  private password = ''
  private wrong = false
  private ctrl = false
  private alt = false

  constructor(
    private readonly vm: VmRec,
    private readonly changed: () => void,
  ) {}

  key(sym: number, down: boolean) {
    if (sym === 0xffe3 || sym === 0xffe4) this.ctrl = down
    if (sym === 0xffe9 || sym === 0xffea || sym === 0xff7e) this.alt = down
    if (!down) return
    // Ctrl+Alt+Del: back to the lock screen.
    if ((sym === 0xffff || sym === 0xff9f) && this.ctrl && this.alt) {
      this.state = 'password'
      this.password = ''
      return this.changed()
    }
    switch (this.state) {
      case 'lock':
        if (sym >= 0xffe1 && sym <= 0xffee) return
        this.state = 'password'
        return this.changed()
      case 'password':
        if (sym === 0xff0d || sym === 0xff8d) {
          if (!this.password) return
          this.state = 'welcome'
          this.changed()
          setTimeout(() => {
            this.state = 'desktop'
            this.changed()
          }, 1400)
          return
        }
        if (sym === 0xff08) this.password = this.password.slice(0, -1)
        else if (sym === 0xff1b) {
          this.password = ''
          this.state = 'lock'
        } else {
          const ch = keysymChar(sym)
          if (!ch || this.ctrl) return
          this.password += ch
        }
        this.wrong = false
        return this.changed()
      default:
        return
    }
  }

  draw(c: Ctx, w: number, h: number) {
    if (this.state === 'desktop') return this.desktop(c, w, h)
    // A dusky, layered backdrop.
    const g = c.createLinearGradient(0, 0, w, h)
    g.addColorStop(0, '#0b2a4a')
    g.addColorStop(0.55, '#1f4f8f')
    g.addColorStop(1, '#5b7fc4')
    c.fillStyle = g
    c.fillRect(0, 0, w, h)
    c.fillStyle = 'rgba(255,255,255,0.06)'
    for (let i = 0; i < 4; i++) {
      c.beginPath()
      c.ellipse(w * (0.2 + i * 0.25), h * (0.85 - i * 0.12), w * 0.45, h * 0.3, -0.35, 0, Math.PI * 2)
      c.fill()
    }
    const font = (px: number, weight = 400) => `${weight} ${px}px 'Segoe UI', 'IBM Plex Sans', system-ui, sans-serif`
    const now = new Date()
    c.textAlign = 'center'
    c.fillStyle = '#fff'
    if (this.state === 'lock') {
      c.font = font(112, 600)
      c.fillText(now.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', hour12: false }), w / 2, h * 0.3)
      c.font = font(30)
      c.fillText(now.toLocaleDateString('en-US', { weekday: 'long', month: 'long', day: 'numeric' }), w / 2, h * 0.3 + 52)
      c.font = font(16)
      c.fillStyle = 'rgba(255,255,255,0.8)'
      c.fillText('Press any key to sign in', w / 2, h * 0.9)
      return
    }
    // The sign-in: the account, and the password box.
    const cy = h * 0.4
    c.fillStyle = 'rgba(255,255,255,0.9)'
    c.beginPath()
    c.arc(w / 2, cy - 40, 72, 0, Math.PI * 2)
    c.fill()
    c.fillStyle = '#5b6b80'
    c.beginPath()
    c.arc(w / 2, cy - 62, 24, 0, Math.PI * 2)
    c.fill()
    c.beginPath()
    c.ellipse(w / 2, cy - 4, 40, 26, 0, Math.PI, 0)
    c.fill()
    c.fillStyle = '#fff'
    c.font = font(30, 600)
    c.fillText(this.vm.account ?? 'Administrator', w / 2, cy + 76)
    if (this.state === 'welcome') {
      c.font = font(22)
      c.fillText('Welcome', w / 2, cy + 130)
      return
    }
    const bw = 300
    const bx = w / 2 - bw / 2
    const by = cy + 104
    c.fillStyle = 'rgba(255,255,255,0.92)'
    c.fillRect(bx, by, bw, 36)
    c.fillStyle = '#1f4f8f'
    c.fillRect(bx, by + 34, bw, 2)
    c.textAlign = 'left'
    c.font = font(this.password ? 22 : 15)
    c.fillStyle = this.password ? '#111' : '#666'
    c.fillText(this.password ? '•'.repeat(Math.min(this.password.length, 24)) : 'Password', bx + 10, by + 25)
    c.textAlign = 'center'
    c.font = font(14)
    c.fillStyle = 'rgba(255,255,255,0.85)'
    c.fillText(this.wrong ? 'The password is incorrect. Try again.' : 'Type any password and press Enter', w / 2, by + 64)
    c.font = font(13)
    c.fillText(computerName(this.vm), w / 2, h - 30)
  }

  private desktop(c: Ctx, w: number, h: number) {
    const g = c.createRadialGradient(w * 0.55, h * 0.45, 40, w * 0.55, h * 0.45, w * 0.8)
    g.addColorStop(0, '#3b82d6')
    g.addColorStop(1, '#0a2a5c')
    c.fillStyle = g
    c.fillRect(0, 0, w, h)
    const font = (px: number, weight = 400) => `${weight} ${px}px 'Segoe UI', 'IBM Plex Sans', system-ui, sans-serif`
    // Icons.
    c.textAlign = 'center'
    for (const [i, label] of ['Recycle Bin', 'This PC'].entries()) {
      const x = 48
      const y = 28 + i * 96
      c.fillStyle = 'rgba(255,255,255,0.85)'
      c.fillRect(x - 18, y, 36, 40)
      c.fillStyle = 'rgba(0,0,0,0.25)'
      c.fillRect(x - 12, y + 8, 24, 24)
      c.fillStyle = '#fff'
      c.font = font(12)
      c.fillText(label, x, y + 58)
    }
    // The taskbar.
    const tb = 48
    c.fillStyle = 'rgba(32,32,36,0.92)'
    c.fillRect(0, h - tb, w, tb)
    const items = 5
    const start = w / 2 - (items * 44) / 2
    for (let i = 0; i < items; i++) {
      const x = start + i * 44 + 8
      c.fillStyle = i === 0 ? '#4aa3ff' : ['#f2c230', '#e5e5e5', '#3fb950', '#c084fc'][i - 1]!
      if (i === 0) {
        for (const [dx, dy] of [
          [0, 0],
          [14, 0],
          [0, 14],
          [14, 14],
        ] as const) c.fillRect(x + dx, h - tb + 10 + dy, 12, 12)
      } else c.fillRect(x + 2, h - tb + 12, 24, 24)
    }
    const now = new Date()
    c.textAlign = 'right'
    c.fillStyle = '#fff'
    c.font = font(12)
    c.fillText(now.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' }), w - 14, h - tb + 21)
    c.fillText(now.toLocaleDateString('en-US'), w - 14, h - tb + 38)
    // A window saying where this is.
    const ww = 460
    const wh = 170
    const wx = w / 2 - ww / 2
    const wy = h / 2 - wh / 2 - 40
    c.fillStyle = 'rgba(243,243,243,0.97)'
    c.fillRect(wx, wy, ww, wh)
    c.fillStyle = '#e6e6e6'
    c.fillRect(wx, wy, ww, 32)
    c.textAlign = 'left'
    c.fillStyle = '#111'
    c.font = font(13)
    c.fillText('About Windows', wx + 12, wy + 21)
    c.font = font(20, 600)
    c.fillText(windowsEdition(this.vm), wx + 20, wy + 70)
    c.font = font(13)
    c.fillStyle = '#333'
    c.fillText(`Computer name: ${computerName(this.vm)}   ·   ${this.vm.vcpus} vCPUs, ${(this.vm.memory_mib / 1024).toFixed(0)} GB`, wx + 20, wy + 100)
    c.fillText('Signed in as ' + (this.vm.account ?? 'Administrator') + '. Ctrl+Alt+Del locks it again.', wx + 20, wy + 126)
  }
}

/** The character a keysym types, if it is a printable one. */
export function keysymChar(sym: number): string | null {
  if (sym >= 0x20 && sym <= 0x7e) return String.fromCharCode(sym)
  if (sym >= 0xa0 && sym <= 0xff) return String.fromCharCode(sym)
  if (sym >= 0x01000100 && sym <= 0x0110ffff) return String.fromCodePoint(sym - 0x01000000)
  return null
}
