// A Linux guest's console: getty's login prompt, then a small shell that
// answers the usual first commands from the VM's record. Used for the
// serial console (ttyS0) and the graphical one's text console (tty1).
import { db, type VmRec } from '../store'
import {
  distro,
  guestDisks,
  interfaces,
  linkLocal,
  lpad,
  memoryMib,
  pad,
  stable,
  uptimeSeconds,
  type Distro,
} from './guest'

type Out = (s: string) => void

const NL = '\r\n'

/** Turns a key's bytes into a stream of input events the shell handles. */
type Mode = 'login' | 'password' | 'shell'

function fmtDate(d = new Date()): string {
  const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat']
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
  const p2 = (n: number) => String(n).padStart(2, '0')
  return `${days[d.getUTCDay()]} ${months[d.getUTCMonth()]} ${lpad(d.getUTCDate(), 2)} ${p2(d.getUTCHours())}:${p2(
    d.getUTCMinutes(),
  )}:${p2(d.getUTCSeconds())} UTC ${d.getUTCFullYear()}`
}

function fmtUptime(secs: number): string {
  const days = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  const parts = days ? `${days} day${days === 1 ? '' : 's'}, ` : ''
  return `${parts}${h ? `${lpad(h, 2)}:${String(m).padStart(2, '0')}` : `${m} min`}`
}

const SYSTEM_DIRS = ['bin', 'boot', 'dev', 'etc', 'home', 'lib', 'lib64', 'media', 'mnt', 'opt', 'proc', 'root', 'run', 'sbin', 'srv', 'sys', 'tmp', 'usr', 'var']

export class LinuxConsole {
  private mode: Mode = 'login'
  private line = ''
  private username = ''
  /** Users the shell runs as: the login, then root after `sudo -i`. */
  private users: string[] = []
  private cwd = ''
  private history: string[] = []
  private historyAt = 0
  private esc = ''
  private readonly d: Distro
  private greeted = false

  constructor(
    private readonly vm: VmRec,
    private readonly out: Out,
    private readonly tty: string,
  ) {
    this.d = distro(vm)
  }

  private get host() {
    return this.vm.name
  }
  private get user() {
    return this.users.at(-1) ?? ''
  }
  private home(user = this.user) {
    return user === 'root' ? '/root' : `/home/${user}`
  }

  /** The banner and login prompt, as getty shows them. */
  greet() {
    this.greeted = true
    this.out(`${NL}${this.issue()}${NL}${NL}${this.host} login: `)
    this.mode = 'login'
    this.line = ''
  }

  private issue() {
    return this.d.id === 'ubuntu'
      ? `${this.d.pretty} ${this.host} ${this.tty}`
      : this.d.id === 'rocky'
        ? `${this.d.pretty}${NL}Kernel ${this.d.kernel} on an x86_64`
        : `Debian GNU/Linux ${this.d.version} ${this.host} ${this.tty}`
  }

  private prompt() {
    const home = this.home()
    const where = this.cwd === home ? '~' : this.cwd.startsWith(`${home}/`) ? `~${this.cwd.slice(home.length)}` : this.cwd
    const sign = this.user === 'root' ? '#' : '$'
    if (this.d.id === 'rocky') this.out(`[${this.user}@${this.host} ${where === '~' ? '~' : where.split('/').pop() || '/'}]${sign} `)
    else this.out(`${this.user}@${this.host}:${where}${sign} `)
  }

  /** Characters typed, as a terminal sends them. */
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
    // Escape sequences: arrows recall history; the rest are ignored.
    if (this.esc) {
      this.esc += ch
      if (this.esc.length === 2 && ch !== '[' && ch !== 'O') this.esc = ''
      else if (this.esc.length > 2 && /[A-Za-z~]/.test(ch)) {
        const seq = this.esc
        this.esc = ''
        if (this.mode === 'shell' && (seq.endsWith('A') || seq.endsWith('B'))) this.recall(seq.endsWith('A') ? -1 : 1)
      }
      return
    }
    if (ch === '\x1b') {
      this.esc = ch
      return
    }
    if (!this.greeted) {
      this.greet()
      if (ch === '\r' || ch === '\n') return
    }
    switch (ch) {
      case '\r':
      case '\n':
        return this.enter()
      case '\x7f':
      case '\b':
        if (this.line) {
          this.line = this.line.slice(0, -1)
          if (this.mode !== 'password') this.out('\b \b')
        }
        return
      case '\x03': // Ctrl-C
        this.out(`^C${NL}`)
        this.line = ''
        return this.mode === 'shell' ? this.prompt() : this.greet()
      case '\x04': // Ctrl-D
        if (this.mode === 'shell' && !this.line) {
          this.out(`logout${NL}`)
          return this.exit()
        }
        return
      case '\x0c': // Ctrl-L
        if (this.mode === 'shell') {
          this.out('\x1b[2J\x1b[H')
          this.prompt()
          this.out(this.line)
        }
        return
      case '\x15': // Ctrl-U
        if (this.mode !== 'password') this.out('\b \b'.repeat(this.line.length))
        this.line = ''
        return
      case '\t':
        return
    }
    if (ch < ' ') return
    this.line += ch
    if (this.mode !== 'password') this.out(ch)
  }

  private recall(dir: number) {
    if (!this.history.length) return
    this.historyAt = Math.max(0, Math.min(this.history.length, this.historyAt + dir))
    const next = this.history[this.historyAt] ?? ''
    this.out('\b \b'.repeat(this.line.length) + next)
    this.line = next
  }

  private enter() {
    const line = this.line
    this.line = ''
    this.out(NL)
    if (this.mode === 'login') {
      this.username = line.trim()
      if (!this.username) return this.out(`${this.host} login: `)
      this.mode = 'password'
      return this.out('Password: ')
    }
    if (this.mode === 'password') {
      if (!/^[a-z_][a-z0-9_.-]*$/i.test(this.username)) {
        this.out(`${NL}Login incorrect${NL}${this.host} login: `)
        this.mode = 'login'
        return
      }
      this.users = [this.username]
      this.cwd = this.home()
      this.mode = 'shell'
      this.out(this.motd())
      return this.prompt()
    }
    const cmd = line.trim()
    if (cmd) {
      this.history.push(cmd)
      if (this.history.length > 100) this.history.shift()
    }
    this.historyAt = this.history.length
    const loggedOut = this.run(cmd)
    if (!loggedOut) this.prompt()
  }

  private motd() {
    const last = new Date(Date.now() - (2 + stable(this.vm, 'last') * 40) * 3600_000)
    const lastLine = `Last login: ${fmtDate(last).replace(' UTC', '')} from 198.51.100.${10 + Math.floor(stable(this.vm, 'from') * 200)}`
    if (this.d.id === 'ubuntu') {
      const m = memoryMib(this.vm)
      const ip = interfaces(this.vm)[0]
      return [
        `Welcome to ${this.d.pretty} (GNU/Linux ${this.d.kernel} x86_64)`,
        '',
        ' * Documentation:  https://help.ubuntu.com',
        ' * Management:     https://landscape.canonical.com',
        ' * Support:        https://ubuntu.com/pro',
        '',
        ` System information as of ${fmtDate()}`,
        '',
        `  System load:  0.${Math.floor(stable(this.vm, 'load') * 40)}              Processes:             ${120 + Math.floor(stable(this.vm, 'procs') * 60)}`,
        `  Usage of /:   ${12 + Math.floor(stable(this.vm, 'disk') * 30)}.4% of ${guestDisks(this.vm)[0]?.gib ?? 10}GB   Users logged in:       0`,
        `  Memory usage: ${Math.round((m.used / m.total) * 100)}%                IPv4 address for ${ip?.name ?? 'enp1s0'}: ${ip?.ip ?? '-'}`,
        '  Swap usage:   0%',
        '',
        lastLine,
        '',
      ].join(NL)
    }
    if (this.d.id === 'rocky') return [lastLine, ''].join(NL)
    return [
      `Linux ${this.host} ${this.d.kernel} ${this.d.build} x86_64`,
      '',
      'The programs included with the Debian GNU/Linux system are free software;',
      'the exact distribution terms for each program are described in the',
      'individual files in /usr/share/doc/*/copyright.',
      '',
      'Debian GNU/Linux comes with ABSOLUTELY NO WARRANTY, to the extent',
      'permitted by applicable law.',
      lastLine,
      '',
    ].join(NL)
  }

  /** Leaves the current shell: back to the login, or out of `sudo -i`. */
  private exit(): boolean {
    this.users.pop()
    if (this.users.length) {
      this.cwd = this.home()
      return false
    }
    this.greet()
    return true
  }

  private print(lines: string | string[]) {
    const text = Array.isArray(lines) ? lines.join(NL) : lines
    if (text) this.out(text.replace(/\r?\n/g, NL) + NL)
  }

  private resolve(path: string): string {
    if (!path || path === '~') return this.home()
    let p = path.startsWith('~/') ? `${this.home()}${path.slice(1)}` : path.startsWith('/') ? path : `${this.cwd}/${path}`
    const parts: string[] = []
    for (const seg of p.split('/')) {
      if (!seg || seg === '.') continue
      if (seg === '..') parts.pop()
      else parts.push(seg)
    }
    p = `/${parts.join('/')}`
    return p
  }

  private dirs(): Map<string, string[]> {
    const users = [...new Set([this.d.user, ...this.users.filter((u) => u !== 'root')])]
    return new Map<string, string[]>([
      ['/', SYSTEM_DIRS],
      ['/home', users],
      ...users.map((u) => [`/home/${u}`, []] as [string, string[]]),
      ['/root', []],
      ['/etc', ['hostname', 'hosts', 'os-release', 'passwd', 'resolv.conf', 'ssh', 'systemd']],
      ['/tmp', []],
      ['/var', ['backups', 'cache', 'lib', 'log', 'tmp']],
      ['/var/log', ['apt', 'cloud-init.log', 'cloud-init-output.log', 'journal', 'wtmp']],
      ['/usr', ['bin', 'include', 'lib', 'local', 'sbin', 'share']],
      ['/opt', []],
      ['/srv', []],
      ['/mnt', []],
    ])
  }

  private files(): Map<string, string> {
    const ips = interfaces(this.vm)
    return new Map([
      ['/etc/hostname', this.host],
      [
        '/etc/os-release',
        [
          `PRETTY_NAME="${this.d.pretty}"`,
          `NAME="${this.d.pretty.replace(/ \d.*$/, '')}"`,
          `VERSION_ID="${this.d.version}"`,
          `VERSION="${this.d.version} (${this.d.codename})"`,
          `VERSION_CODENAME=${this.d.codename.toLowerCase().replace(/ /g, '')}`,
          `ID=${this.d.id}`,
          this.d.id === 'debian' ? 'HOME_URL="https://www.debian.org/"' : this.d.id === 'ubuntu' ? 'HOME_URL="https://www.ubuntu.com/"' : 'HOME_URL="https://rockylinux.org/"',
        ].join('\n'),
      ],
      ['/etc/hosts', `127.0.0.1 localhost\n127.0.1.1 ${this.host}\n${ips.map((i) => `${i.ip} ${this.host}`).join('\n')}`],
      ['/etc/resolv.conf', `nameserver ${ips[0]?.gateway ?? '10.0.0.1'}\nsearch .`],
    ])
  }

  /** Runs a command line; says whether it logged out. */
  private run(cmdline: string): boolean {
    if (!cmdline) return false
    const [cmd, ...args] = cmdline.split(/\s+/) as [string, ...string[]]
    const vm = db.vms.find((v) => v.id === this.vm.id) ?? this.vm
    const root = this.user === 'root'
    switch (cmd) {
      case 'help':
        this.print([
          'This is a demo console. Try:',
          '  uname -a  hostname  uptime  whoami  id  date  nproc',
          '  ip a  df -h  free -m  lsblk  ps  top  systemctl status',
          '  ls  cd  pwd  cat /etc/os-release  sudo -i  clear  exit',
        ])
        return false
      case 'exit':
      case 'logout':
        if (cmd === 'logout' && this.users.length > 1) {
          this.print('bash: logout: not login shell: use `exit\'')
          return false
        }
        if (this.users.length === 1) this.out(`logout${NL}`)
        return this.exit()
      case 'sudo':
        if (args[0] === '-i' || args[0] === '-s' || args[0] === 'su' || args[0] === 'bash') {
          if (!root) {
            this.users.push('root')
            this.cwd = '/root'
          }
          return false
        }
        if (!args.length) {
          this.print('usage: sudo -h | -K | -k | -V')
          return false
        }
        return this.runAs('root', args.join(' '))
      case 'su':
        if (!root) this.print('Password: \r\nsu: Authentication failure')
        return false
      case 'clear':
        this.out('\x1b[2J\x1b[H')
        return false
      case 'echo':
        this.print(args.join(' ').replace(/\$HOSTNAME/g, this.host).replace(/\$USER/g, this.user).replace(/\$HOME/g, this.home()).replace(/^["']|["']$/g, ''))
        return false
      case 'hostname':
        this.print(args.includes('-I') ? interfaces(vm).map((i) => i.ip).join(' ') + ' ' : this.host)
        return false
      case 'whoami':
        this.print(this.user)
        return false
      case 'id':
        this.print(
          root
            ? 'uid=0(root) gid=0(root) groups=0(root)'
            : `uid=1000(${this.user}) gid=1000(${this.user}) groups=1000(${this.user}),4(adm),24(cdrom),27(sudo),30(dip),44(video),46(plugdev)`,
        )
        return false
      case 'uname':
        this.print(
          args.includes('-a')
            ? `Linux ${this.host} ${this.d.kernel} ${this.d.build} x86_64 GNU/Linux`
            : args.includes('-r')
              ? this.d.kernel
              : args.includes('-n')
                ? this.host
                : 'Linux',
        )
        return false
      case 'uptime': {
        const load = (k: string) => (stable(vm, k) * 0.6).toFixed(2)
        this.print(` ${fmtDate().split(' ')[3]} up ${fmtUptime(uptimeSeconds(vm))},  1 user,  load average: ${load('l1')}, ${load('l5')}, ${load('l15')}`)
        return false
      }
      case 'date':
        this.print(fmtDate())
        return false
      case 'nproc':
        this.print(String(vm.vcpus))
        return false
      case 'pwd':
        this.print(this.cwd)
        return false
      case 'cd': {
        const to = this.resolve(args[0] ?? '~')
        if (!this.dirs().has(to)) this.print(`-bash: cd: ${args[0]}: No such file or directory`)
        else if (to === '/root' && !root) this.print(`-bash: cd: ${args[0]}: Permission denied`)
        else this.cwd = to
        return false
      }
      case 'ls':
      case 'll':
      case 'dir': {
        const target = this.resolve(args.find((a) => !a.startsWith('-')) ?? this.cwd)
        const entries = this.dirs().get(target)
        if (!entries) {
          const f = this.files().has(target)
          this.print(f ? target.split('/').pop()! : `ls: cannot access '${target}': No such file or directory`)
        } else if (target === '/root' && !root) this.print("ls: cannot open directory '/root': Permission denied")
        else if (entries.length) this.print(entries.join('  '))
        return false
      }
      case 'cat': {
        for (const a of args) {
          const f = this.files().get(this.resolve(a))
          this.print(f ?? `cat: ${a}: ${this.dirs().has(this.resolve(a)) ? 'Is a directory' : 'No such file or directory'}`)
        }
        return false
      }
      case 'ip':
        if (/^(a|addr|address)$/.test(args[0] ?? '')) this.print(this.ipAddr(vm))
        else if (/^(r|route)$/.test(args[0] ?? '')) {
          const i = interfaces(vm)
          this.print([
            ...(i[0] ? [`default via ${i[0].gateway} dev ${i[0].name} proto dhcp src ${i[0].ip} metric 100`] : []),
            ...i.map((x) => `${x.ip.replace(/\.\d+$/, '.0')}/${x.prefix} dev ${x.name} proto kernel scope link src ${x.ip} metric 100`),
          ])
        } else if (/^(l|link)$/.test(args[0] ?? '')) this.print(this.ipAddr(vm).filter((l) => !/inet|valid_lft/.test(l)))
        else this.print('Usage: ip [ OPTIONS ] OBJECT { COMMAND | help }\r\nwhere  OBJECT := { address | link | route | ... }')
        return false
      case 'ifconfig':
        this.print('-bash: ifconfig: command not found')
        return false
      case 'df':
        this.print(this.df(vm))
        return false
      case 'free':
        this.print(this.free(vm, args.includes('-h')))
        return false
      case 'lsblk':
        this.print(this.lsblk(vm))
        return false
      case 'ps':
        this.print(this.ps(args.some((a) => a.includes('aux') || a === '-ef')))
        return false
      case 'top':
      case 'htop':
        this.print(this.top(vm))
        return false
      case 'systemctl':
        this.print(this.systemctl(vm, args))
        return false
      case 'reboot':
      case 'poweroff':
      case 'shutdown':
      case 'halt':
        this.print(root ? 'In the demo, use the VM page\'s power actions.' : `${cmd}: must be superuser.`)
        return false
      case 'apt':
      case 'apt-get':
      case 'dnf':
      case 'yum':
        this.print(root ? `${cmd}: the demo has no network to fetch packages from.` : 'E: Could not open lock file - open (13: Permission denied)\r\nE: Are you root?')
        return false
      case 'ping':
        this.print(args.length ? `PING ${args.at(-1)}: the demo has no network.` : 'ping: usage error: Destination address required')
        return false
      case 'history':
        this.print(this.history.map((h, i) => `${lpad(i + 1, 5)}  ${h}`))
        return false
      default:
        this.print(`-bash: ${cmd}: command not found`)
        return false
    }
  }

  private runAs(user: string, cmdline: string): boolean {
    this.users.push(user)
    try {
      this.run(cmdline)
    } finally {
      this.users.pop()
    }
    return false
  }

  private ipAddr(vm: VmRec): string[] {
    const out = [
      '1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default qlen 1000',
      '    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00',
      '    inet 127.0.0.1/8 scope host lo',
      '       valid_lft forever preferred_lft forever',
      '    inet6 ::1/128 scope host noprefixroute',
      '       valid_lft forever preferred_lft forever',
    ]
    interfaces(vm).forEach((i, n) => {
      const bcast = i.prefix === 24 ? i.ip.replace(/\.\d+$/, '.255') : i.ip
      out.push(
        `${n + 2}: ${i.name}: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1442 qdisc fq_codel state UP group default qlen 1000`,
        `    link/ether ${i.mac} brd ff:ff:ff:ff:ff:ff`,
        `    inet ${i.ip}/${i.prefix} metric 100 brd ${bcast} scope global dynamic ${i.name}`,
        '       valid_lft 85912sec preferred_lft 85912sec',
        `    inet6 ${linkLocal(i.mac)}/64 scope link`,
        '       valid_lft forever preferred_lft forever',
      )
    })
    return out
  }

  private df(vm: VmRec): string[] {
    const boot = guestDisks(vm)[0]
    const size = boot?.gib ?? 10
    const used = Math.max(1.2, size * (0.1 + stable(vm, 'disk') * 0.3))
    const mem = memoryMib(vm)
    const g = (x: number) => (x >= 10 ? `${Math.round(x)}G` : `${x.toFixed(1)}G`)
    const m = (x: number) => (x >= 1024 ? g(x / 1024) : `${Math.round(x)}M`)
    return [
      'Filesystem      Size  Used Avail Use% Mounted on',
      `udev            ${pad(m(mem.total / 2), 5)}    0 ${pad(m(mem.total / 2), 5)}   0% /dev`,
      `tmpfs           ${pad(m(mem.total / 5), 5)} ${pad('620K', 5)}${pad(m(mem.total / 5), 5)}   1% /run`,
      `/dev/vda1       ${pad(g(size - 0.2), 5)} ${pad(g(used), 5)}${pad(g(size - 0.2 - used), 5)} ${lpad(Math.round((used / size) * 100), 3)}% /`,
      `tmpfs           ${pad(m(mem.total / 2), 5)}    0 ${pad(m(mem.total / 2), 5)}   0% /dev/shm`,
      `tmpfs           ${pad('5.0M', 5)}    0 ${pad('5.0M', 5)}   0% /run/lock`,
      `/dev/vda15      ${pad('124M', 5)} ${pad('12M', 5)}${pad('113M', 5)}  10% /boot/efi`,
      `tmpfs           ${pad(m(mem.total / 10), 5)}    0 ${pad(m(mem.total / 10), 5)}   0% /run/user/1000`,
    ]
  }

  private free(vm: VmRec, human: boolean): string[] {
    const m = memoryMib(vm)
    const f = (x: number) => (human ? (x >= 1024 ? `${(x / 1024).toFixed(1)}Gi` : `${x}Mi`) : String(x))
    return [
      '               total        used        free      shared  buff/cache   available',
      `Mem:    ${lpad(f(m.total), 12)}${lpad(f(m.used), 12)}${lpad(f(m.free), 12)}${lpad(f(Math.round(m.total * 0.01)), 12)}${lpad(f(m.cache), 12)}${lpad(f(m.available), 12)}`,
      `Swap:   ${lpad(f(0), 12)}${lpad(f(0), 12)}${lpad(f(0), 12)}`,
    ]
  }

  private lsblk(vm: VmRec): string[] {
    const out = ['NAME    MAJ:MIN RM  SIZE RO TYPE MOUNTPOINTS']
    let sd = 0
    for (const d of guestDisks(vm)) {
      if (d.boot) {
        out.push(
          `vda     254:0    0 ${lpad(`${d.gib}G`, 4)}  0 disk `,
          `├─vda1  254:1    0 ${lpad(`${(d.gib - 0.1).toFixed(1)}G`, 4)}  0 part /`,
          '├─vda14 254:14   0    3M  0 part ',
          '└─vda15 254:15   0  124M  0 part /boot/efi',
        )
      } else {
        out.push(`${pad(d.dev, 8)}8:${lpad(16 * ++sd, 3)}    0 ${lpad(`${d.gib}G`, 4)}  0 disk `)
      }
    }
    out.push('sr0      11:0    1  366K  0 rom  ')
    return out
  }

  private ps(all: boolean): string[] {
    if (!all) return ['    PID TTY          TIME CMD', `   ${1200 + this.history.length} ${pad(this.tty === 'tty1' ? 'tty1' : 'ttyS0', 8)} 00:00:00 bash`, `   ${1300 + this.history.length} ${pad(this.tty === 'tty1' ? 'tty1' : 'ttyS0', 8)} 00:00:00 ps`]
    return [
      'USER         PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND',
      'root           1  0.0  0.3  22404 13312 ?        Ss   08:12   0:02 /sbin/init',
      'root         246  0.0  0.4  50308 16220 ?        Ss   08:12   0:00 /usr/lib/systemd/systemd-journald',
      'root         289  0.0  0.1  27384  7040 ?        Ss   08:12   0:00 /usr/lib/systemd/systemd-udevd',
      'systemd+     412  0.0  0.2  19084  9728 ?        Ss   08:12   0:00 /usr/lib/systemd/systemd-networkd',
      'systemd+     430  0.0  0.3  21436 12544 ?        Ss   08:12   0:00 /usr/lib/systemd/systemd-resolved',
      'root         512  0.0  0.1   6608  2688 ?        Ss   08:12   0:00 /usr/sbin/cron -f',
      'root         520  0.0  0.1 236768  6400 ?        Ssl  08:12   0:03 /usr/sbin/qemu-ga',
      'root         538  0.0  0.2  12116  8064 ?        Ss   08:12   0:00 sshd: /usr/sbin/sshd -D',
      `root         601  0.0  0.1   5896  1920 ${this.tty === 'tty1' ? 'ttyS0' : 'tty1 '}    Ss+  08:12   0:00 /sbin/agetty -o -- \\u --noreset --noclear - linux`,
      `${pad(this.user.slice(0, 8), 8)}    1201  0.0  0.1   8640  5376 ${pad(this.tty, 8)} Ss   ${fmtDate().split(' ')[3]!.slice(0, 5)}   0:00 -bash`,
      `${pad(this.user.slice(0, 8), 8)}    1302  0.0  0.1  11084  4480 ${pad(this.tty, 8)} R+   ${fmtDate().split(' ')[3]!.slice(0, 5)}   0:00 ps aux`,
    ]
  }

  private top(vm: VmRec): string[] {
    const m = memoryMib(vm)
    const idle = (96 + stable(vm, 'idle') * 3.5).toFixed(1)
    return [
      `top - ${fmtDate().split(' ')[3]} up ${fmtUptime(uptimeSeconds(vm))},  1 user,  load average: 0.08, 0.05, 0.01`,
      'Tasks: 121 total,   1 running, 120 sleeping,   0 stopped,   0 zombie',
      `%Cpu(s):  ${(100 - Number(idle)) / 2 < 1 ? '0.4' : '1.1'} us,  0.3 sy,  0.0 ni, ${idle} id,  0.0 wa,  0.0 hi,  0.1 si,  0.0 st`,
      `MiB Mem : ${lpad(m.total.toFixed(1), 8)} total, ${lpad(m.free.toFixed(1), 8)} free, ${lpad(m.used.toFixed(1), 8)} used, ${lpad(m.cache.toFixed(1), 8)} buff/cache`,
      `MiB Swap:      0.0 total,      0.0 free,      0.0 used. ${lpad(m.available.toFixed(1), 8)} avail Mem`,
      '',
      '    PID USER      PR  NI    VIRT    RES    SHR S  %CPU  %MEM     TIME+ COMMAND',
      '    520 root      20   0  236768   6400   5504 S   0.3   0.1   0:03.41 qemu-ga',
      '      1 root      20   0   22404  13312   9472 S   0.0   0.3   0:02.17 systemd',
      '    246 root      20   0   50308  16220  15196 S   0.0   0.4   0:00.48 systemd-journal',
      '    412 systemd+  20   0   19084   9728   8576 S   0.0   0.2   0:00.21 systemd-network',
      '    538 root      20   0   12116   8064   6912 S   0.0   0.2   0:00.02 sshd',
      '(A snapshot: the demo\'s top does not refresh.)',
    ]
  }

  private systemctl(vm: VmRec, args: string[]): string[] {
    const verb = args[0] ?? 'status'
    if (verb === 'status' && args.length <= 1) {
      return [
        `● ${this.host}`,
        '    State: running',
        `    Units: ${290 + Math.floor(stable(vm, 'units') * 40)} loaded (incl. loaded aliases)`,
        '     Jobs: 0 queued',
        '   Failed: 0 units',
        `    Since: ${fmtDate(new Date(Date.now() - uptimeSeconds(vm) * 1000))}; ${fmtUptime(uptimeSeconds(vm))} ago`,
        '  systemd: 257.8-1',
        '   CGroup: /',
        '           ├─init.scope',
        '           │ └─1 /sbin/init',
        '           ├─system.slice',
        '           │ ├─cron.service',
        '           │ ├─qemu-guest-agent.service',
        '           │ ├─ssh.service',
        '           │ ├─systemd-journald.service',
        '           │ └─systemd-networkd.service',
        '           └─user.slice',
      ]
    }
    if (verb === 'status') {
      const unit = args[1]!.replace(/\.service$/, '')
      const known = ['ssh', 'sshd', 'cron', 'qemu-guest-agent', 'systemd-networkd', 'systemd-journald', 'cloud-init']
      if (!known.includes(unit)) return [`Unit ${unit}.service could not be found.`]
      return [
        `● ${unit}.service`,
        `     Loaded: loaded (/usr/lib/systemd/system/${unit}.service; enabled; preset: enabled)`,
        `     Active: active (running) since ${fmtDate(new Date(Date.now() - uptimeSeconds(vm) * 1000))}; ${fmtUptime(uptimeSeconds(vm))} ago`,
        `   Main PID: ${400 + Math.floor(stable(vm, unit) * 200)} (${unit})`,
      ]
    }
    if (['start', 'stop', 'restart', 'enable', 'disable', 'reload'].includes(verb)) {
      return this.user === 'root' ? [] : ['Failed to ' + verb + ' ' + (args[1] ?? '') + ': Access denied', 'See system logs and \'systemctl status\' for details.']
    }
    return [`Unknown command verb '${verb}'.`]
  }
}
