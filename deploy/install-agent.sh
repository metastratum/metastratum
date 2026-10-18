#!/usr/bin/env bash
# Installs a metastratum host on this machine, with what it needs: libvirt
# and QEMU, Open vSwitch and OVN's host part, and the agent, enrolled with
# a controller; VMs keep their disks on this machine. It also gives OVN an
# external network that this machine NATs to the outside, which VMs reach
# the outside through (install-controller.sh declares it to metastratum).
# On the controller's own machine (install-controller.sh ran here), it
# enrolls with that controller by itself. Safe to run again: what is set up
# already is left as it is.
#
# Supported: Debian 13, Fedora 43 and later, RHEL, Rocky Linux and
# AlmaLinux 10, and Arch Linux, on x86-64 with hardware virtualisation. On
# Arch, OVN is built from source (it is not packaged) and the system is
# upgraded first (pacman -Syu), as Arch requires. RHEL needs its Fast
# Datapath repository (Open vSwitch and OVN) through a subscription.
#
# Usage: install-agent.sh [options]
#   --controller URL      the controller's agent endpoint (https://ADDRESS:8443),
#                         several comma-separated; default: the controller on
#                         this machine
#   --address IP          this machine's address, which the controller and
#                         other hosts reach it on (default: the one its route
#                         to the controller leaves from)
#   --external-cidr CIDR  the external network NATed here (default
#                         10.250.250.0/24, as install-controller.sh declares
#                         it), or "none"
#   --version VERSION     metastratum release to install (default: the latest)
#   --packages DIR        install metastratum from the packages in DIR (as
#                         built by deploy/packaging/build.sh) instead
#   -y, --yes             do not ask before changing the system
#
# The join token (from the controller: mst-controller join-token, or
# Infrastructure → Hosts → Add host): MST_JOIN_TOKEN, or asked for. Not
# needed on the controller's machine, nor once the host is enrolled.
set -euo pipefail

ROLE=agent

address="" controllers="" ext_cidr=10.250.250.0/24 version="" packages="" assume_yes=0

while [[ $# -gt 0 ]]; do
  case $1 in
    --controller) controllers=${2:?}; shift ;;
    --address) address=${2:?}; shift ;;
    --external-cidr) ext_cidr=${2:?}; shift ;;
    --version) version=${2#v}; shift ;;
    --packages) packages=$(cd "${2:?}" && pwd); shift ;;
    -y | --yes) assume_yes=1 ;;
    -h | --help) sed -n '/^# Usage:/,/^# needed/s/^# \{0,1\}//p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
  esac
  shift
done

# The controller: the one given, or this machine's own. The address
# defaults to the one the route there leaves from.
local_controller=0
if [[ -z $controllers ]]; then
  if [[ -e /etc/metastratum/controller.toml ]] && command -v mst-controller >/dev/null; then
    local_controller=1
  elif [[ ! -e /etc/metastratum/agent.toml ]]; then
    echo "no controller on this machine: give one with --controller https://ADDRESS:8443" >&2
    exit 1
  fi
elif [[ -z $address ]]; then
  first=${controllers%%,*} first=${first#https://} first=${first%%[:/]*}
  address=$(ip -4 -o route get "$(getent ahostsv4 "$first" | awk '{print $1; exit}')" 2>/dev/null |
    sed -n 's/.* src \([0-9.]*\).*/\1/p')
fi

# ---- shared with install-controller.sh (deploy/check-installers.sh compares) ----

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
  ubuntu) family=debian; warn "$PRETTY_NAME is not tested; going on as for Debian" ;;
  fedora) family=fedora; ((major >= 43)) || die "Fedora 43 or later is needed (this is $PRETTY_NAME)" ;;
  rhel | rocky | almalinux) family=el; ((major >= 10)) || die "$NAME 10 or later is needed (this is $PRETTY_NAME)" ;;
  arch) family=arch ;;
  *) die "$PRETTY_NAME is not supported: Debian 13, Fedora 43+, RHEL/Rocky/AlmaLinux 10 or Arch Linux" ;;
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

# <n> random bytes, in hex (without pipelines head would cut short).
# shellcheck disable=SC2329 # shared: not every installer needs it
random_hex() { od -An -tx1 -N"$1" /dev/urandom | tr -d ' \n'; }

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
    # packages that install on RHEL 10).
    [[ $(printf '%s\n' 0.4.0 "$version" | sort -V | sed -n 1p) == 0.4.0 ]] ||
      die "the installers need metastratum 0.4.0 or later (not $version)"
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

if [[ $ext_cidr != none ]]; then
  [[ $ext_cidr =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)\.0/24$ ]] ||
    die "--external-cidr takes a /24 network (like 10.250.250.0/24), or none"
  ext_prefix=${ext_cidr%.0/24}
  if ip -4 -o addr show | grep -v ' br-ext ' | grep " inet $ext_prefix\." >/dev/null; then
    die "$ext_cidr is in use on this machine already; choose another with --external-cidr"
  fi
fi
((local_controller)) && controllers=https://$address:8443
grep -qwE 'vmx|svm' /proc/cpuinfo ||
  warn "this CPU shows no hardware virtualisation (VT-x/AMD-V): VMs will not start. Switch it on in the firmware settings."

cat <<EOF
This installs a metastratum host on $host ($PRETTY_NAME), its VMs on local
disks, enrolled with ${controllers:-the controller it is enrolled with}:

  - from the distribution: libvirt and QEMU, Open vSwitch and OVN's host part$([[ $family == arch ]] && printf '\n    (after a full system upgrade; OVN built from source)')
  - metastratum-agent ${version:-(the latest release)}$([[ -n $packages ]] && printf ' from %s' "$packages")
EOF
[[ $ext_cidr != none ]] && cat <<EOF
  - an Open vSwitch bridge br-ext with $ext_prefix.1 on it, and NAT from
    $ext_cidr to the outside: IP forwarding gets switched on
EOF
echo
confirm

# The join token, asked for now rather than half-way.
token=""
if [[ ! -e $CONF/agent.toml ]] && ((!local_controller)); then
  token=${MST_JOIN_TOKEN:-}
  if [[ -z $token ]]; then
    [[ -t 0 ]] || die "set MST_JOIN_TOKEN to a join token from the controller"
    read -r -s -p "Join token (mst-controller join-token, or Infrastructure → Hosts → Add host): " token; echo
  fi
  [[ $token == mstjoin.* ]] || die "that is not a join token"
fi

install_packages() {
  log "installing libvirt, QEMU, Open vSwitch and OVN"
  case $family in
    debian)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -q
      apt-get install -y -q openvswitch-switch ovn-host qemu-system-x86 qemu-utils \
        libvirt-daemon-system libvirt-clients xorriso ovmf nftables curl ca-certificates
      ;;
    fedora)
      dnf install -y -q openvswitch ovn-host qemu-kvm qemu-img libvirt-daemon-kvm libvirt-client \
        xorriso edk2-ovmf nftables curl
      ;;
    el)
      el_repos
      local ovn ovs
      ovn=$(ovn_pkg host) ovs=$(ovs_pkg)
      [[ -n $ovn && -n $ovs ]] || die "no Open vSwitch or OVN packages in the configured repositories"
      dnf install -y -q "$ovn" "$ovs" qemu-kvm qemu-img libvirt-daemon-kvm libvirt-client \
        xorriso edk2-ovmf nftables curl
      ;;
    arch)
      pacman -Syu --needed --noconfirm openvswitch qemu-base qemu-img qemu-hw-display-virtio-vga \
        qemu-hw-display-virtio-gpu libvirt libisoburn edk2-ovmf nftables curl libarchive
      build_ovn
      ovn_unit ovn-controller "OVN controller" \
        "After=ovs-vswitchd.service
Requires=ovs-vswitchd.service" \
        "Type=forking
PIDFile=/run/ovn/ovn-controller.pid
ExecStart=/usr/local/share/ovn/scripts/ovn-ctl start_controller --no-monitor
ExecStop=/usr/local/share/ovn/scripts/ovn-ctl stop_controller
Restart=on-failure"
      systemctl daemon-reload
      ;;
  esac
}

start_services() {
  # An upgraded kernel (Arch's pacman -Syu) takes the running one's modules
  # away: Open vSwitch's cannot load until the machine runs the new one.
  [[ -d /lib/modules/$(uname -r) ]] ||
    die "the kernel was upgraded: reboot, then run $0 again"
  log "starting Open vSwitch and libvirt"
  case $family in
    debian) systemctl enable --now -q openvswitch-switch ;;
    fedora | el) systemctl enable --now -q openvswitch ;;
    # ovsdb-server comes with ovs-vswitchd (its unit is static).
    arch) systemctl enable --now -q ovs-vswitchd ;;
  esac
  if systemctl list-unit-files -q virtqemud.socket >/dev/null 2>&1; then
    # libvirt's per-driver daemons, started on first use.
    systemctl enable --now -q virtqemud.socket virtsecretd.socket virtlogd.socket \
      virtnodedevd.socket virtstoraged.socket
  else
    systemctl enable --now -q libvirtd
  fi
  [[ -c /dev/kvm ]] || warn "there is no /dev/kvm: VMs will not start until hardware virtualisation works"
}

setup_ovs() {
  log "connecting Open vSwitch to OVN"
  local id
  id=$(ovs-vsctl --if-exists get Open_vSwitch . external_ids:system-id | tr -d '"')
  if [[ -z $id ]]; then
    id=$host
    # Some distributions' Open vSwitch sets its system-id from this at start.
    printf '%s\n' "$id" >/etc/openvswitch/system-id.conf
  fi
  ovs-vsctl set Open_vSwitch . "external_ids:system-id=$id" \
    external_ids:ovn-encap-type=geneve "external_ids:ovn-encap-ip=$address"
  if [[ $ext_cidr != none ]]; then
    ovs-vsctl --may-exist add-br br-ext
    local mappings
    mappings=$(ovs-vsctl --if-exists get Open_vSwitch . external_ids:ovn-bridge-mappings | tr -d '"')
    if [[ ,$mappings, != *,external:br-ext,* ]]; then
      ovs-vsctl set Open_vSwitch . "external_ids:ovn-bridge-mappings=\"${mappings:+$mappings,}external:br-ext\""
    fi
  fi
  systemctl enable -q ovn-controller
  systemctl restart ovn-controller
  if systemctl -q is-active firewalld; then
    # OVN's tunnels and live migrations between hosts.
    firewall-cmd -q --permanent --add-port=6081/udp --add-port=49152-49215/tcp
    firewall-cmd -q --reload
  fi
}

# The external network: the bridge's own port has its first address, and
# this machine routes (NATs) the rest out.
setup_external() {
  [[ $ext_cidr == none ]] && return
  log "routing $ext_cidr out through this machine"
  install -d /usr/local/lib/metastratum
  cat >/usr/local/lib/metastratum/external <<EOF
#!/bin/sh
# Written by metastratum's install-agent.sh: this host's side of the
# external network $ext_cidr (Open vSwitch bridge br-ext), NATed out.
set -eu
case \${1:-up} in
  up)
    i=0
    until ip link show br-ext >/dev/null 2>&1; do
      i=\$((i + 1)); [ \$i -lt 60 ] || { echo "no bridge br-ext" >&2; exit 1; }
      sleep 1
    done
    ip addr replace $ext_prefix.1/24 dev br-ext
    ip link set br-ext up
    sysctl -qw net.ipv4.ip_forward=1
    # With firewalld, its zones do this (br-ext trusted, masquerading).
    if ! systemctl -q is-active firewalld; then
      nft -f - <<'NFT'
table ip metastratum {}
delete table ip metastratum
table ip metastratum {
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr $ext_cidr ip daddr != $ext_cidr masquerade
  }
  chain forward {
    type filter hook forward priority filter; policy accept;
    iifname "br-ext" accept
    oifname "br-ext" ct state established,related accept
  }
}
NFT
    fi
    ;;
  down) nft delete table ip metastratum 2>/dev/null || true ;;
esac
EOF
  chmod 755 /usr/local/lib/metastratum/external
  cat >/etc/systemd/system/metastratum-external.service <<EOF
# Written by metastratum's install-agent.sh.
[Unit]
Description=metastratum's external network $ext_cidr, NATed to the outside
After=network-online.target openvswitch-switch.service openvswitch.service ovs-vswitchd.service
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/lib/metastratum/external up
ExecStop=/usr/local/lib/metastratum/external down

[Install]
WantedBy=multi-user.target
EOF
  printf 'net.ipv4.ip_forward = 1\n' >/etc/sysctl.d/90-metastratum.conf
  systemctl daemon-reload
  systemctl enable -q metastratum-external
  systemctl restart metastratum-external
  if systemctl -q is-active firewalld; then
    firewall-cmd -q --permanent --zone=trusted --add-interface=br-ext
    firewall-cmd -q --permanent --add-masquerade
    firewall-cmd -q --reload
  fi
  if systemctl -q is-active docker; then
    warn "Docker runs here: its firewall rules (FORWARD policy DROP) may keep VMs from the outside. Allow br-ext:
  iptables -I DOCKER-USER -i br-ext -j ACCEPT
  iptables -I DOCKER-USER -o br-ext -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"
  fi
}

setup_agent() {
  if [[ ! -e $CONF/agent.toml ]]; then
    log "enrolling this machine with $controllers"
    ((local_controller)) && token=$(mst-controller join-token --cluster default)
    local list
    list=$(printf '%s' "$controllers" | sed 's/[^,][^,]*/"&"/g; s/,/, /g')
    install -d -m 755 "$CONF"
    (
      umask 077
      cat >"$CONF/agent.toml" <<EOF
# metastratum host agent, set up by install-agent.sh; each setting is
# explained in /usr/share/doc/metastratum-agent/examples/agent.toml.
controllers = [$list]
address = "$address"
# Single-use; the agent keeps its own identity once enrolled.
join_token = "$token"
EOF
    )
  fi
  local since
  since=$(date '+%Y-%m-%d %H:%M:%S')
  systemctl enable -q mst-agent
  systemctl restart mst-agent
  local _
  for _ in $(seq 60); do
    journalctl -q -u mst-agent --since "$since" | grep 'session established' >/dev/null && return
    sleep 2
  done
  die "the agent did not reach the controller; see: journalctl -u mst-agent"
}

install_packages
start_services
install_metastratum
setup_ovs
setup_external
setup_agent

echo
log "this machine is a metastratum host now: Infrastructure → Hosts lists it"
[[ $ext_cidr != none ]] && cat <<EOF

  VMs reach the outside through the external network $ext_cidr, NATed by
  this machine; floating IPs from it are reachable from here.
EOF
exit 0
