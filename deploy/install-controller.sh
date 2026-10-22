#!/usr/bin/env bash
# Installs a metastratum controller on this machine, with what it needs:
# PostgreSQL, OVN's central databases (over TLS), the controller and its
# first administrator, and Caddy as the web UI's HTTPS proxy. Then hosts
# join it: run install-agent.sh on each (on this machine too, for a
# single-machine installation; it enrolls itself with the controller here).
# Safe to run again: what is set up already is left as it is.
#
# What runs here already is used, not replaced: a PostgreSQL server gets a
# database and a user of its own (and a pg_hba.conf line for them, if it
# needs one; it is reloaded, not restarted). Before changing anything, it
# looks for what it would clash with (ports in use, OVN databases another
# deployment uses, a database left from an earlier installation) and stops
# if it finds any.
#
# Supported: Debian 13, Ubuntu 26.04 and later, Fedora 43 and later, RHEL,
# Rocky Linux and AlmaLinux 10, and Arch Linux, on x86-64. On Arch, OVN is built from source
# (it is not packaged) and the system is upgraded first (pacman -Syu), as
# Arch requires. RHEL needs its Fast Datapath repository (Open vSwitch and
# OVN) through a subscription.
#
# Usage: install-controller.sh [options]
#   --address IP          this machine's address, which hosts reach it on
#                         (default: the one its default route leaves from)
#   --domain NAME         serve the web UI at https://NAME with a certificate
#                         from Let's Encrypt (NAME must resolve here, and ports
#                         80 and 443 be reachable). Default: https://ADDRESS,
#                         with a certificate from Caddy's own CA
#   --https-port PORT     the web UI's port (default 443): another keeps clear
#                         of a web server here, and needs no port 80 (unless
#                         with --domain, for Let's Encrypt)
#   --no-proxy            no HTTPS proxy: the UI stays on 127.0.0.1:7820
#   --external-cidr CIDR  the external network VMs reach the outside through
#                         (default 10.250.250.0/24, as install-agent.sh NATs
#                         it on each host), or "none"
#   --dns IP[,IP...]      the name servers VMs get by DHCP (default: this
#                         machine's, unless they are on loopback), or "none"
#   --ovn-northbound REMOTES  use OVN's central databases elsewhere
#                         (ssl:HOST:6641, comma-separated) instead of
#                         installing them here; the certificates their
#                         servers need are written for you to install there
#   --ovn-southbound REMOTES  their southbound databases (default: the same
#                         hosts, port 6642)
#   --version VERSION     metastratum release to install (default: the latest)
#   --packages DIR        install metastratum from the packages in DIR (as
#                         built by deploy/packaging/build.sh) instead
#   --force               go ahead although it would take over what someone
#                         else set up (OVN databases in use)
#   -y, --yes             do not ask before changing the system
#
# The administrator's password: MST_ADMIN_PASSWORD, or asked for; without a
# terminal, made up and kept in /root/metastratum-admin-password.
# MST_DATABASE_URL (postgres://USER:PASSWORD@HOST/DB) uses a PostgreSQL
# database elsewhere, empty or this controller's own, instead of one here.
set -euo pipefail

ROLE=controller
API=http://127.0.0.1:7820
IMAGE_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2

address="" domain="" proxy=1 ext_cidr=10.250.250.0/24 dns="" version="" packages="" assume_yes=0
ovn_nb="" ovn_sb="" force=0 https_port=443

while [[ $# -gt 0 ]]; do
  case $1 in
    --address) address=${2:?}; shift ;;
    --domain) domain=${2:?}; shift ;;
    --https-port) https_port=${2:?}; shift ;;
    --no-proxy) proxy=0 ;;
    --external-cidr) ext_cidr=${2:?}; shift ;;
    --dns) dns=${2:?}; shift ;;
    --ovn-northbound) ovn_nb=${2:?}; shift ;;
    --ovn-southbound) ovn_sb=${2:?}; shift ;;
    # Earlier installers added an image; adding one now waits for a host.
    --no-image) ;;
    --force) force=1 ;;
    --version) version=${2#v}; shift ;;
    --packages) packages=$(cd "${2:?}" && pwd); shift ;;
    -y | --yes) assume_yes=1 ;;
    -h | --help) sed -n '/^# Usage:/,/^# database elsewhere/s/^# \{0,1\}//p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
  esac
  shift
done

# ---- shared with install-agent.sh (deploy/check-installers.sh compares) ----

REPO=metastratum/metastratum
STATE=/var/lib/metastratum-install
CONF=/etc/metastratum
# OVN for Arch, built from its release tag (pinned to its commit).
OVN_VERSION=26.09.0
OVN_COMMIT=da7afdb1bffa5ff752f2e8ae50ffd7aecd2907f3
OVN_OVS_COMMIT=ba8649c1c3aec12a6f93877064c40d63ebbf3545

if [[ -t 1 ]]; then
  log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }
  warn() { printf '\e[1;33mwarning:\e[0m %s\n' "$*" >&2; }
  die() { printf '\e[1;31merror:\e[0m %s\n' "$*" >&2; exit 1; }
else
  log() { printf '==> %s\n' "$*"; }
  warn() { printf 'warning: %s\n' "$*" >&2; }
  die() { printf 'error: %s\n' "$*" >&2; exit 1; }
fi

[[ $EUID -eq 0 ]] || die "run it as root"
[[ $(uname -m) == x86_64 ]] || die "metastratum's packages are for x86-64"
[[ -d /run/systemd/system ]] || die "it needs systemd"
[[ -r /etc/os-release ]] || die "no /etc/os-release: unknown distribution"
# shellcheck source=/dev/null
. /etc/os-release
# Rolling releases (Arch) have no VERSION_ID.
major=${VERSION_ID:-0}
major=${major%%.*}
case $ID in
  debian) family=debian; ((major >= 13)) || die "Debian 13 or later is needed (this is $PRETTY_NAME)" ;;
  ubuntu) family=debian; ((major >= 26)) || die "Ubuntu 26.04 or later is needed (this is $PRETTY_NAME)" ;;
  fedora) family=fedora; ((major >= 43)) || die "Fedora 43 or later is needed (this is $PRETTY_NAME)" ;;
  rhel | rocky | almalinux) family=el; ((major >= 10)) || die "$NAME 10 or later is needed (this is $PRETTY_NAME)" ;;
  arch) family=arch ;;
  *) die "$PRETTY_NAME is not supported: Debian 13, Ubuntu 26.04+, Fedora 43+, RHEL/Rocky/AlmaLinux 10 or Arch Linux" ;;
esac

if [[ -z $address ]]; then
  address=$(ip -4 -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
  [[ -n $address ]] || die "cannot tell this machine's address; give it with --address"
fi
ip -4 -o addr show | grep " inet $address/" >/dev/null || die "$address is not an address of this machine"
host=$(uname -n)
host=${host%%.*}
[[ -n $host ]] || die "this machine has no host name"
mkdir -p "$STATE"
chmod 700 "$STATE"
# One installer at a time (both may run on one machine).
exec 9>"$STATE/lock"
flock -n 9 || die "another metastratum installer is running"

# What is on the machine already that the installer would clash with,
# found before it changes anything: problems stop it; takeovers (changing
# what someone else set up) stop it unless --force; notes are said.
problems=() takeovers=() notes=()
problem() { problems+=("$*"); }
takeover() { takeovers+=("$*"); }
note() { notes+=("$*"); }
report_conflicts() {
  local p
  for p in ${notes[@]+"${notes[@]}"}; do warn "$p"; done
  for p in ${takeovers[@]+"${takeovers[@]}"}; do
    if ((force)); then warn "going ahead (--force): $p"; fi
  done
  ((${#problems[@]} == 0 && (${#takeovers[@]} == 0 || force))) && return
  {
    echo "This machine has what the installer would clash with:"
    for p in ${problems[@]+"${problems[@]}"}; do printf '  - %s\n' "$p"; done
    for p in ${takeovers[@]+"${takeovers[@]}"}; do printf '  - %s (--force to go ahead anyway)\n' "$p"; done
  } >&2
  die "nothing was changed"
}

# Whether ufw (Ubuntu's firewall, which may be on elsewhere too) is on.
ufw_active() { command -v ufw >/dev/null && ufw status 2>/dev/null | grep '^Status: active' >/dev/null; }

# Asks before going on, unless --yes.
confirm() {
  ((assume_yes)) && return
  [[ -t 0 ]] || die "no terminal to ask on: run it with --yes"
  local ok
  read -r -p "Go ahead? [y/N] " ok
  [[ $ok == [yY]* ]] || exit 1
}

# RHEL's Open vSwitch and OVN, and Caddy, are not in its own repositories.
el_repos() {
  if [[ $ID == rhel ]]; then
    local arch
    arch=$(uname -m)
    subscription-manager repos --enable "codeready-builder-for-rhel-$major-$arch-rpms" \
      --enable "fast-datapath-for-rhel-$major-$arch-rpms" >/dev/null ||
      die "RHEL needs the Fast Datapath repository for Open vSwitch and OVN (a subscription that includes it)"
    rpm -q epel-release >/dev/null 2>&1 ||
      dnf install -y -q "https://dl.fedoraproject.org/pub/epel/epel-release-latest-$major.noarch.rpm"
  else
    # Open vSwitch and OVN from the CentOS NFV SIG; Caddy from EPEL.
    dnf install -y -q epel-release centos-release-nfv-openvswitch
  fi
}

# The newest of the versioned packages RHEL's Open vSwitch and OVN come as
# (openvswitch3.5, ovn26.09-host...): ovn_pkg <part>, ovs_pkg.
ovn_pkg() {
  dnf -q repoquery --latest-limit 1 --qf '%{name}\n' "ovn[0-9]*-$1" 2>/dev/null |
    grep -E "^ovn[0-9.]+-$1\$" | sort -V | tail -1
}
ovs_pkg() {
  dnf -q repoquery --latest-limit 1 --qf '%{name}\n' 'openvswitch[0-9]*' 2>/dev/null |
    grep -E '^openvswitch[0-9.]+$' | sort -V | tail -1
}

# Arch has no OVN package: build the pinned release, with the Open vSwitch
# it is made for, into /usr/local.
build_ovn() {
  if [[ -x /usr/local/bin/ovn-controller ]] &&
    /usr/local/bin/ovn-controller --version | grep " $OVN_VERSION\$" >/dev/null; then
    return
  fi
  log "building OVN $OVN_VERSION from source (takes a few minutes)"
  pacman -S --needed --noconfirm base-devel git python autoconf automake libtool libcap-ng openssl
  local src=$STATE/ovn-src
  rm -rf -- "$STATE/ovn-src"
  git clone -q --depth 1 --branch "v$OVN_VERSION" https://github.com/ovn-org/ovn.git "$src"
  [[ $(git -C "$src" rev-parse HEAD) == "$OVN_COMMIT" ]] || die "OVN v$OVN_VERSION is not the commit expected"
  git -C "$src" submodule -q update --init --depth 1
  [[ $(git -C "$src/ovs" rev-parse HEAD) == "$OVN_OVS_COMMIT" ]] || die "OVN's Open vSwitch is not the commit expected"
  local opts=(--prefix=/usr/local --localstatedir=/var --sysconfdir=/etc --enable-ssl)
  (
    cd "$src/ovs" && ./boot.sh && ./configure -q "${opts[@]}" && make -s -j"$(nproc)"
    cd "$src" && ./boot.sh && ./configure -q "${opts[@]}" --with-ovs-source="$src/ovs" &&
      make -s -j"$(nproc)" && make -s install
  ) >"$STATE/ovn-build.log" 2>&1 || die "building OVN failed; see $STATE/ovn-build.log"
  rm -rf -- "$STATE/ovn-src"
  # ovn-ctl reads Open vSwitch's shell library from beside its own: the
  # distribution's, which knows where its daemons are.
  install -d /usr/local/share/openvswitch
  ln -sfn /usr/share/openvswitch/scripts /usr/local/share/openvswitch/scripts
}

# A unit for OVN built from source, as Debian's: ovn_unit <name> <description>
# <[Unit] lines> <[Service] lines>.
ovn_unit() {
  cat >"/etc/systemd/system/$1.service" <<EOF
# Written by metastratum's installer, for OVN built from source.
[Unit]
Description=$2
$3

[Service]
$4

[Install]
WantedBy=multi-user.target
EOF
}

# Installs metastratum's package for $ROLE: from --packages, or the release.
install_metastratum() {
  local dir=$STATE/packages kind file
  case $family in debian) kind=deb ;; *) kind=rpm ;; esac
  rm -rf -- "$STATE/packages" && mkdir -p "$dir"
  if [[ -n $packages ]]; then
    log "installing metastratum-$ROLE from $packages"
    file=$(find "$packages" -maxdepth 1 -name "metastratum-${ROLE}[-_]*.$kind" | sort -V | tail -1)
    [[ -n $file ]] || die "no metastratum-$ROLE .$kind package in $packages"
    cp "$file" "$dir/"
  else
    if [[ -z $version ]]; then
      version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
      version=${version##*/v}
      [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "cannot tell metastratum's latest release"
    fi
    # Older releases lack what the installers set up ([network] settings,
    # packages that install on RHEL 10), and 0.4's agents open libvirt's TLS
    # listener, which these installers no longer check for.
    [[ $(printf '%s\n' 0.5.0 "$version" | sort -V | sed -n 1p) == 0.5.0 ]] ||
      die "the installers need metastratum 0.5.0 or later (not $version)"
    log "installing metastratum-$ROLE $version"
    case $kind in
      deb) file=metastratum-${ROLE}_${version}_amd64.deb ;;
      rpm) file=metastratum-$ROLE-$version-1.x86_64.rpm ;;
    esac
    curl -fsSL -o "$dir/$file" "https://github.com/$REPO/releases/download/v$version/$file" ||
      die "cannot download metastratum-$ROLE $version ($file)"
  fi
  file=$(find "$dir" -name "*.$kind")
  case $family in
    debian) apt-get install -y -q "$file" ;;
    fedora | el) dnf install -y -q "$file" ;;
    arch) install_rpm "$file" ;;
  esac
}

# Arch: the RPM's files (a static binary, its unit, the web UI) put in
# place; the list kept in $STATE/<role>.files, to remove them by.
install_rpm() {
  local root=$STATE/unpacked f
  rm -rf -- "$STATE/unpacked" && mkdir -p "$root"
  bsdtar -xf "$1" -C "$root"
  (cd "$root" && find . -type f -o -type l) | sed 's/^\.//' | sort >"$STATE/$ROLE.files"
  while read -r f; do
    if [[ -L $root$f ]]; then
      mkdir -p "$(dirname "$f")" && cp -P "$root$f" "$f"
    else
      install -D -m "$(stat -c %a "$root$f")" "$root$f" "$f"
    fi
  done <"$STATE/$ROLE.files"
  install -d -m 755 "$CONF"
  rm -rf -- "$STATE/unpacked"
  systemctl daemon-reload
}

# ---- end of the shared part ----

# The programs listening on TCP port <port> here, one per line: name, and
# the systemd service it runs in (none for one started from a shell).
port_users() {
  local prog pid
  # Nothing listening is no failure.
  ss -Htlnp "sport = :$1" 2>/dev/null | grep -o '("[^"]*",pid=[0-9]*' | sed 's/^("//; s/",pid=/ /' |
    sort -u | while read -r prog pid; do
      printf '%s %s\n' "$prog" "$(sed -n 's|.*/\([^/]*\.service\)$|\1|p' "/proc/$pid/cgroup" 2>/dev/null | sed -n 1p)"
    done || true
}

# check_port <port> <what it is for> <services that may hold it...>: by the
# service, not the program's name (a controller started by hand is not the
# one this installs).
check_port() {
  local port=$1 what=$2 prog unit
  shift 2
  while read -r prog unit; do
    if [[ -n $prog && " $* " != *" $unit "* ]]; then
      problem "port $port ($what) is in use by $prog${unit:+ ($unit)}"
      return
    fi
  done < <(port_users "$port")
}

# <n> random bytes, in hex (without pipelines head would cut short).
random_hex() { od -An -tx1 -N"$1" /dev/urandom | tr -d ' \n'; }

if [[ $ext_cidr != none ]]; then
  [[ $ext_cidr =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)\.0/24$ ]] ||
    die "--external-cidr takes a /24 network (like 10.250.250.0/24), or none"
  ext_prefix=${ext_cidr%.0/24}
fi
# The name servers for VMs: this machine's own (systemd-resolved's upstream
# ones rather than its stub), except on loopback, which VMs cannot reach.
if [[ -z $dns ]]; then
  resolv=/etc/resolv.conf
  [[ -r /run/systemd/resolve/resolv.conf ]] && resolv=/run/systemd/resolve/resolv.conf
  dns=$(awk '$1 == "nameserver" && $2 ~ /^[0-9.]+$/ && $2 !~ /^127\./ { print $2 }' "$resolv" | paste -sd, -)
  [[ -n $dns ]] || warn "this machine has no name servers VMs could use: they will resolve no names (see --dns)"
fi
[[ $dns == none ]] && dns=""
[[ -z $dns || $dns =~ ^[0-9.]+(,[0-9.]+)*$ ]] || die "--dns takes IPv4 addresses, comma-separated, or none"

# OVN's central databases: here, or (--ovn-northbound) elsewhere.
local_ovn=1
if [[ -n $ovn_nb ]]; then
  local_ovn=0
  [[ -n $ovn_sb ]] || ovn_sb=$(printf '%s' "$ovn_nb" | sed -E 's/:6641(,|$)/:6642\1/g; s/^tcp:/ssl:/; s/,tcp:/,ssl:/g')
  for r in ${ovn_nb//,/ } ${ovn_sb//,/ }; do
    [[ $r =~ ^(ssl|tcp):[^:]+:[0-9]+$ ]] || die "OVN servers are given as ssl:HOST:PORT (not $r)"
  done
  [[ $ovn_sb != *tcp:* ]] || die "the southbound database must be reached over TLS (ssl:HOST:PORT)"
else
  ovn_nb=ssl:$address:6641 ovn_sb=ssl:$address:6642
fi
# TOML's list of the comma-separated values in $1.
toml_list() { printf '%s' "$1" | sed 's/[^,][^,]*/"&"/g; s/,/, /g'; }

# PostgreSQL: the one given (MST_DATABASE_URL), the one running here, or a
# new one.
postgres_ports() {
  ss -Htlnp 2>/dev/null | awk '/"(postgres|postmaster)"/ { n = split($4, a, ":"); print a[n] }' | sort -un
}
pg_port=5432
if [[ -n ${MST_DATABASE_URL:-} ]]; then
  pg=external
  [[ $MST_DATABASE_URL =~ ^postgres(ql)?:// ]] || die "MST_DATABASE_URL is a postgres:// URL"
else
  mapfile -t ports < <(postgres_ports)
  case ${#ports[@]} in
    0) pg=new ;;
    1) pg=existing pg_port=${ports[0]} ;;
    *) die "PostgreSQL servers run here on ports ${ports[*]}: say which database to use with MST_DATABASE_URL" ;;
  esac
fi
# SQL as PostgreSQL's superuser, on this machine.
pgsql() { (cd /tmp && runuser -u postgres -- psql -X -p "$pg_port" -v ON_ERROR_STOP=1 -qtA "$@"); }
# A metastratum database this installer did not make (it notes the ones
# it makes, for a run cut short to carry on with).
leftover_database() {
  if [[ ! -e $STATE/database ]] &&
    [[ $(pgsql -c "SELECT 1 FROM pg_database WHERE datname = 'metastratum'") == 1 ]]; then
    die "PostgreSQL has a database named metastratum, but there is no $CONF/controller.toml: put that file and master.key back (they hold the database's password and the key its secrets are sealed with), or drop the database to start over (dropdb metastratum)"
  fi
}

# What it would clash with.
first=1
[[ -e $CONF/controller.toml ]] && first=0
if ! [[ $https_port =~ ^[0-9]+$ ]] || ((https_port < 1 || https_port > 65535)); then
  die "--https-port takes a port number"
fi
check_port 7820 "the controller's web UI and API" mst-controller.service
check_port 7821 "the controller's endpoint for hosts" mst-controller.service
if ((proxy)); then
  check_port "$https_port" "the web UI's HTTPS proxy" caddy.service
  # Port 80: for redirects to the UI on 443, and Let's Encrypt's checks.
  if ((https_port == 443)) || [[ -n $domain ]]; then
    check_port 80 "the web UI's HTTPS proxy, for redirects and certificates" caddy.service
  fi
  if printf '%s\n' ${problems[@]+"${problems[@]}"} | grep -E "^port (80|$https_port) " >/dev/null; then
    note "--https-port serves the web UI on another port, and --no-proxy leaves it on 127.0.0.1:7820 for a proxy of your own"
  fi
fi
# The web UI's address.
ui_url=https://${domain:-$address}
((https_port == 443)) || ui_url+=:$https_port
if ((local_ovn)); then
  # OVN's database servers, as each distribution runs them.
  ovn_units="ovn-central.service ovn-northd.service ovn-ovsdb-server-nb.service ovn-ovsdb-server-sb.service"
  # shellcheck disable=SC2086 # one service each
  check_port 6641 "OVN's northbound database" $ovn_units
  # shellcheck disable=SC2086
  check_port 6642 "OVN's southbound database" $ovn_units
  if [[ ! -e /etc/ovn/metastratum/nb.pem ]] && command -v ovn-nbctl >/dev/null &&
    ovn-nbctl --timeout=5 show >/dev/null 2>&1; then
    in_use=$(
      for t in Logical_Switch Logical_Router; do ovn-nbctl --timeout=5 --bare --columns=_uuid list $t; done |
        grep -c . || true
    )
    listeners=$(ovn-nbctl --timeout=5 --bare --columns=target list Connection | grep . | paste -sd' ' - || true)
    if ((in_use > 0)) || [[ -n $listeners ]]; then
      takeover "OVN's databases here are in use ($in_use logical switches and routers${listeners:+, listening on $listeners}): metastratum would share them, and serve them over TLS on ports 6641 and 6642 only. Or use other ones: --ovn-northbound"
    fi
  fi
fi
if [[ $pg == existing ]] && ((first)) && [[ ! -e $STATE/database ]] && command -v psql >/dev/null; then
  [[ $(pgsql -c "SELECT 1 FROM pg_database WHERE datname = 'metastratum'") == 1 ]] &&
    problem "PostgreSQL has a database named metastratum, but there is no $CONF/controller.toml: put that file and master.key back, or drop the database to start over (dropdb metastratum)"
fi
report_conflicts

selinux=0
if ((proxy)) && command -v getenforce >/dev/null && [[ $(getenforce) == Enforcing ]]; then
  selinux=1
fi
cat <<EOF
This installs a metastratum controller on $host ($PRETTY_NAME):

  - PostgreSQL: $(case $pg in
  external) printf 'the database at MST_DATABASE_URL' ;;
  existing) printf 'the server running here (port %s) gets a database and a user' "$pg_port" ;;
  new) printf 'installed from the distribution' ;;
esac)
  - OVN's central databases: $(if ((local_ovn)); then printf 'installed here, served over TLS'; else printf 'at %s' "$ovn_nb"; fi)$([[ $family == arch ]] && ((local_ovn)) && printf ' (built from source,\n    after a full system upgrade)')
  - metastratum-controller ${version:-(the latest release)}$([[ -n $packages ]] && printf ' from %s' "$packages")
  - the web UI at $(if ((proxy)); then printf '%s, through Caddy' "$ui_url"; else printf 'http://127.0.0.1:7820'; fi)
  - hosts reach it at $address (port 7821$( ((local_ovn)) && printf ', and 6642 for OVN'))
  - VMs get ${dns:-no} name server(s) by DHCP
EOF
((selinux)) && cat <<EOF
  - SELinux lets web servers connect to the network (httpd_can_network_connect),
    for Caddy to reach the controller
EOF
{ systemctl -q is-active firewalld || ufw_active; } && cat <<EOF
  - $(systemctl -q is-active firewalld && printf firewalld || printf ufw) lets in what is listed above
EOF
echo
confirm

# The administrator's password, asked for now rather than half-way.
admin_password_file=/root/metastratum-admin-password
admin_password=${MST_ADMIN_PASSWORD:-}
if [[ ! -e $STATE/bootstrapped && -z $admin_password ]]; then
  if [[ -t 0 ]]; then
    while :; do
      read -r -s -p "Password for the administrator (admin), at least 12 characters: " admin_password; echo
      ((${#admin_password} >= 12)) || { warn "too short"; continue; }
      read -r -s -p "Again: " again; echo
      [[ $admin_password == "$again" ]] && break
      warn "they differ"
    done
  else
    admin_password=$(random_hex 12)
    (umask 077; printf '%s\n' "$admin_password" >"$admin_password_file")
    log "made up a password for admin: it is in $admin_password_file"
  fi
fi
if [[ ! -e $STATE/bootstrapped ]]; then
  ((${#admin_password} >= 12)) || die "the administrator's password needs at least 12 characters"
fi

install_packages() {
  local pkgs=(curl jq openssl) what=()
  ((proxy)) && pkgs+=(caddy) && what+=(Caddy)
  case $pg in
    new) what+=(PostgreSQL) ;;
    existing) what+=("PostgreSQL's client") ;;
  esac
  ((local_ovn)) && what+=("OVN's central databases")
  ((${#what[@]})) && log "installing $(printf '%s, ' "${what[@]}" | sed 's/, $//')"
  case $family in
    debian)
      case $pg in new) pkgs+=(postgresql) ;; existing) pkgs+=(postgresql-client) ;; esac
      ((local_ovn)) && pkgs+=(ovn-central)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -q
      apt-get install -y -q ca-certificates "${pkgs[@]}"
      ;;
    fedora | el)
      case $pg in new) pkgs+=(postgresql-server postgresql-contrib) ;; existing) pkgs+=(postgresql) ;; esac
      if ((local_ovn)); then
        if [[ $family == el ]]; then
          el_repos
          local ovn ovs
          ovn=$(ovn_pkg central) ovs=$(ovs_pkg)
          [[ -n $ovn && -n $ovs ]] || die "no Open vSwitch or OVN packages in the configured repositories"
          # ovsdb-server, which serves OVN's databases, comes with Open vSwitch.
          pkgs+=("$ovn" "$ovs")
        else
          pkgs+=(ovn-central openvswitch)
        fi
      elif [[ $family == el ]] && ((proxy)); then
        el_repos
      fi
      dnf install -y -q "${pkgs[@]}"
      ;;
    arch)
      [[ $pg == external ]] || pkgs+=(postgresql)
      ((local_ovn)) && pkgs+=(openvswitch)
      pacman -Syu --needed --noconfirm libarchive "${pkgs[@]}"
      if ((local_ovn)); then
        build_ovn
        local ctl=/usr/local/share/ovn/scripts/ovn-ctl db
        for db in nb sb; do
          ovn_unit "ovn-ovsdb-server-$db" "OVN ${db^^} database" "After=network.target" \
            "ExecStart=$ctl run_${db}_ovsdb
ExecStop=$ctl stop_${db}_ovsdb
Restart=on-failure"
        done
        ovn_unit ovn-northd "OVN northd" \
          "After=ovn-ovsdb-server-nb.service ovn-ovsdb-server-sb.service
Requires=ovn-ovsdb-server-nb.service ovn-ovsdb-server-sb.service" \
          "Type=forking
PIDFile=/run/ovn/ovn-northd.pid
ExecStart=$ctl start_northd --ovn-manage-ovsdb=no --no-monitor
ExecStop=$ctl stop_northd --ovn-manage-ovsdb=no
Restart=on-failure"
        systemctl daemon-reload
      fi
      ;;
  esac
}

# Starts what was installed; what ran already is left running as it is.
start_services() {
  if [[ $pg == new ]]; then
    log "starting PostgreSQL"
    case $family in
      debian) systemctl enable --now -q postgresql ;;
      fedora | el)
        [[ -e /var/lib/pgsql/data/PG_VERSION ]] || postgresql-setup --initdb >/dev/null
        systemctl enable --now -q postgresql
        ;;
      arch)
        if [[ ! -e /var/lib/postgres/data/PG_VERSION ]]; then
          (cd /tmp && runuser -u postgres -- initdb -D /var/lib/postgres/data -E UTF8 \
            --auth-local=peer --auth-host=scram-sha-256 >/dev/null)
        fi
        systemctl enable --now -q postgresql
        ;;
    esac
    local _
    for _ in $(seq 30); do
      mapfile -t ports < <(postgres_ports)
      ((${#ports[@]})) && break
      sleep 1
    done
    ((${#ports[@]} == 1)) || die "PostgreSQL does not listen on TCP here (listen_addresses); see its log"
    pg_port=${ports[0]}
  fi
  if ((local_ovn)); then
    log "starting OVN's central databases"
    case $family in
      debian) systemctl enable --now -q ovn-central ;;
      fedora | el) systemctl enable --now -q ovn-northd ;;
      arch) systemctl enable --now -q ovn-ovsdb-server-nb ovn-ovsdb-server-sb ovn-northd ;;
    esac
  fi
}

# Whether metastratum's user can sign in over TCP, as the controller will.
db_login_works() {
  PGPASSWORD=$db_password PGCONNECT_TIMEOUT=5 psql -X -h 127.0.0.1 -p "$pg_port" -U metastratum \
    -d metastratum -qtAc 'SELECT 1' >/dev/null 2>&1
}

# A database and a user for the controller, in the PostgreSQL here.
setup_database() {
  [[ $pg == external || -e $CONF/controller.toml ]] && return
  leftover_database
  log "creating the database (PostgreSQL on port $pg_port)"
  db_password=$(random_hex 16)
  local verb=CREATE
  [[ $(pgsql -c "SELECT 1 FROM pg_roles WHERE rolname = 'metastratum'") == 1 ]] && verb=ALTER
  # Through stdin: the password stays off command lines.
  pgsql <<<"$verb ROLE metastratum LOGIN PASSWORD '$db_password';" >/dev/null
  if [[ $(pgsql -c "SELECT 1 FROM pg_database WHERE datname = 'metastratum'") != 1 ]]; then
    pgsql -c "CREATE DATABASE metastratum OWNER metastratum" >/dev/null
  fi
  touch "$STATE/database"
  db_login_works && return
  # The server's rules do not let it in: one of its own, first, for just
  # this database and user from this machine. Reloaded, not restarted.
  local hba method=scram-sha-256
  hba=$(pgsql -c 'SHOW hba_file')
  [[ $(pgsql -c 'SHOW password_encryption') == md5 ]] && method=md5
  log "letting the controller in: a line for it in $hba"
  [[ -e $hba.metastratum-orig ]] || cp -p "$hba" "$hba.metastratum-orig"
  local rules
  rules=$(
    printf '# metastratum'"'"'s controller (install-controller.sh)\n'
    printf 'host metastratum metastratum 127.0.0.1/32 %s\n' "$method"
    printf 'host metastratum metastratum ::1/128 %s\n' "$method"
    cat "$hba"
  )
  # Rewritten in place: it keeps its owner, mode and SELinux label.
  printf '%s\n' "$rules" >"$hba"
  pgsql -c 'SELECT pg_reload_conf()' >/dev/null
  sleep 1
  db_login_works || die "metastratum's user cannot sign in to PostgreSQL at 127.0.0.1:$pg_port: does it listen there (listen_addresses)? See its log, and $hba"
}

setup_controller() {
  install -d -m 755 "$CONF"
  if [[ ! -e $CONF/controller.toml ]]; then
    log "configuring the controller"
    local url
    if [[ $pg == external ]]; then
      url=$MST_DATABASE_URL
    else
      url=postgres://metastratum:$db_password@127.0.0.1:$pg_port/metastratum
    fi
    (
      umask 077
      mst-controller gen-master-key >"$CONF/master.key"
      cat >"$CONF/controller.toml" <<EOF
# metastratum controller, set up by install-controller.sh; each setting is
# explained in /usr/share/doc/metastratum-controller/examples/controller.toml.
database_url = "$url"

[api]
# The web UI and API, behind the HTTPS proxy on this machine.
listen = "127.0.0.1:7820"
trusted_proxies = ["127.0.0.1"]

[agents]
listen = "0.0.0.0:7821"
peer_address = "$address:7821"

[ovn]
northbound = [$(toml_list "$ovn_nb")]
southbound = [$(toml_list "$ovn_sb")]

[network]
# Name servers VMs get by DHCP.
dns_servers = [$(toml_list "$dns")]
EOF
    )
  fi
  if ((local_ovn)) && [[ ! -e /etc/ovn/metastratum/nb.pem ]]; then
    log "serving OVN's databases over TLS"
    local dir
    dir=$(mktemp -d)
    mst-controller ovn-certificate --name "$host" --address "$address,127.0.0.1" \
      --out-dir "$dir" >/dev/null
    sh "$dir/setup.sh" >/dev/null
    rm -rf -- "$dir"
  elif ((!local_ovn)); then
    ovn_bundles
  fi
  if [[ ! -e $STATE/bootstrapped ]]; then
    log "creating the administrator (admin)"
    MST_ADMIN_PASSWORD=$admin_password mst-controller bootstrap >/dev/null
    touch "$STATE/bootstrapped"
  fi
  systemctl enable -q mst-controller
  systemctl restart mst-controller
  local _
  for _ in $(seq 60); do
    curl -fsS -o /dev/null "$API/healthz" 2>/dev/null && break
    sleep 1
  done
  curl -fsS -o /dev/null "$API/healthz" || die "the controller does not answer; see: journalctl -u mst-controller"
  # Hosts' agents, and their ovn-controller.
  if systemctl -q is-active firewalld; then
    firewall-cmd -q --permanent --add-port=7821/tcp
    ((local_ovn)) && firewall-cmd -q --permanent --add-port=6642/tcp
    firewall-cmd -q --reload
  fi
  if ufw_active; then
    ufw allow 7821/tcp comment "metastratum: host agents" >/dev/null
    ((local_ovn)) && ufw allow 6642/tcp comment "metastratum: OVN southbound" >/dev/null
  fi
}

# OVN elsewhere: what each of its servers needs for TLS, from this
# controller's CAs, in /root/metastratum-ovn/<host>/ (with setup.sh).
ovn_bundles() {
  ovn_hosts=()
  local r h nb sb
  for r in ${ovn_nb//,/ }; do
    [[ $r == ssl:* ]] || continue
    h=${r#ssl:} h=${h%:*} nb=${r##*:}
    sb=$(tr , '\n' <<<"$ovn_sb" | sed -n "s/^ssl:$h:\([0-9]*\)$/\1/p" | sed -n 1p)
    ovn_hosts+=("$h")
    [[ -e /root/metastratum-ovn/$h/setup.sh ]] && continue
    log "writing certificates for OVN's server $h"
    install -d -m 700 /root/metastratum-ovn
    mst-controller ovn-certificate --name "$h" --address "$h" --nb-port "$nb" \
      --sb-port "${sb:-6642}" --out-dir "/root/metastratum-ovn/$h" >/dev/null
  done
}

setup_proxy() {
  ((proxy)) || return 0
  local file=/etc/caddy/Caddyfile
  if [[ -e $file ]] && ! grep -q 'metastratum' "$file"; then
    # The distributions' Caddyfile serves a placeholder page; anything else
    # is someone's, and stays.
    if grep -qE '^\s*(root|file_server)' "$file" && ! grep -qE '^\s*reverse_proxy' "$file"; then
      mv "$file" "$file.orig"
    else
      warn "$file is in use: add this site to it yourself:
  $ui_url { reverse_proxy 127.0.0.1:7820 }"
      return
    fi
  fi
  log "serving the web UI at $ui_url"
  install -d /etc/caddy
  {
    printf '# metastratum'"'"'s web UI and API, written by install-controller.sh.\n'
    printf '{\n\tskip_install_trust\n'
    # Off the HTTPS port, no redirects from port 80 (which may be another's).
    ((https_port == 443)) || [[ -n $domain ]] || printf '\tauto_https disable_redirects\n'
    printf '}\n\n%s {\n' "$ui_url"
    [[ -z $domain ]] && printf '\t# No name to get a certificate for: one from Caddy'"'"'s own CA.\n\ttls internal\n'
    printf '\treverse_proxy 127.0.0.1:7820\n}\n'
  } >"$file"
  ((selinux)) && setsebool -P httpd_can_network_connect 1
  systemctl enable -q caddy
  systemctl restart caddy
  if ufw_active; then
    ufw allow "$https_port/tcp" comment "metastratum: web UI" >/dev/null
    if ((https_port == 443)) || [[ -n $domain ]]; then
      ufw allow 80/tcp comment "metastratum: web UI (redirects, certificates)" >/dev/null
    fi
  fi
  if systemctl -q is-active firewalld; then
    if ((https_port == 443)); then
      firewall-cmd -q --permanent --add-service=https --add-service=http
    else
      firewall-cmd -q --permanent --add-port="$https_port/tcp"
      [[ -z $domain ]] || firewall-cmd -q --permanent --add-service=http
    fi
    firewall-cmd -q --reload
  fi
}

api() { # api METHOD PATH [JSON]
  curl -fsS -b "$jar" -X "$1" -H 'content-type: application/json' ${3:+--data "$3"} "$API/api/v1$2"
}

# Through the API: the external network the hosts NAT, and the provider's
# default VPC on it. Not an image: one is kept by a host, and none has
# joined yet.
provision() {
  [[ -e $STATE/provisioned ]] && return
  if [[ -z $admin_password ]]; then
    [[ -r $admin_password_file ]] || {
      warn "the external network was not set up: run again with MST_ADMIN_PASSWORD set"
      return
    }
    admin_password=$(cat "$admin_password_file")
  fi
  jar=$(mktemp)
  trap 'rm -f -- "$jar"' EXIT
  # The password goes to jq and curl through the environment and stdin.
  MST_PW=$admin_password jq -n '{username: "admin", password: env.MST_PW}' |
    curl -fsS -c "$jar" -H 'content-type: application/json' --data @- \
      -o /dev/null "$API/api/v1/auth/login" || die "cannot sign in as admin"
  if [[ $ext_cidr != none ]]; then
    log "adding the external network $ext_cidr, and the default VPC on it"
    local ext vpc
    ext=$(api GET /external-networks | jq -r '.[] | select(.name == "outside") | .id')
    if [[ -z $ext ]]; then
      ext=$(api POST /external-networks "$(jq -n --arg c "$ext_cidr" --arg p "$ext_prefix" \
        '{name: "outside", physical_network: "external", cidr: $c, gateway: ($p + ".1"),
          pool_start: ($p + ".100"), pool_end: ($p + ".250")}')" | jq -r .id)
    fi
    vpc=$(api GET /vpcs | jq -r '.[] | select(.name == "default") | .id' | sed -n 1p)
    if [[ -z $vpc ]]; then
      api POST /vpcs "$(jq -n --arg e "$ext" '{name: "default", external_network_id: $e}')" >/dev/null
    else
      api PATCH "/vpcs/$vpc" "$(jq -n --arg e "$ext" '{external_network_id: $e}')" >/dev/null
    fi
  fi
  touch "$STATE/provisioned"
}

install_packages
start_services
install_metastratum
setup_database
setup_controller
setup_proxy
provision

echo
log "the controller is installed"
cat <<EOF

  Web UI:  $(if ((proxy)); then printf '%s' "$ui_url"; else printf 'http://127.0.0.1:7820 (forward it: ssh -L 7820:127.0.0.1:7820 root@%s)' "$address"; fi)
  Sign in: admin$([[ -e $admin_password_file ]] && printf ', with the password in %s' "$admin_password_file")
EOF
if ((proxy)) && [[ -z $domain ]]; then
  ca=$(find /var/lib/caddy -path '*authorities/local/root.crt' 2>/dev/null | sed -n 1p)
  cat <<EOF
  The UI's certificate is from Caddy's own CA; browsers warn about it unless
  they trust that CA: ${ca:-/var/lib/caddy/.../pki/authorities/local/root.crt}
EOF
fi
if ((!local_ovn)) && ((${#ovn_hosts[@]})); then
  cat <<EOF

OVN's servers need certificates from this controller before it, and hosts,
can use them. For each, copy its directory there and run setup.sh as root:
EOF
  for h in "${ovn_hosts[@]}"; do
    printf '\n  scp -r /root/metastratum-ovn/%s root@%s:metastratum-ovn\n  ssh root@%s sh metastratum-ovn/setup.sh\n' "$h" "$h" "$h"
  done
fi
cat <<EOF

Next, hosts: run install-agent.sh on each. On this machine it enrolls with
this controller by itself; elsewhere give it a join token from here
(mst-controller join-token --cluster default, or Infrastructure → Hosts):

  MST_JOIN_TOKEN=... install-agent.sh --controller https://$address:7821

Then add an image for VMs (Images → Add image), for example
$IMAGE_URL

Keep copies of $CONF/controller.toml and master.key somewhere safe.
Check the installation with: mst-controller doctor
EOF
