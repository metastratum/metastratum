#!/usr/bin/env bash
# Installs a metastratum controller on this machine, with what it needs:
# PostgreSQL, OVN's central databases (over TLS), the controller and its
# first administrator, and Caddy as the web UI's HTTPS proxy. Then hosts
# join it: run install-agent.sh on each (on this machine too, for a
# single-machine installation; it enrolls itself with the controller here).
# Safe to run again: what is set up already is left as it is.
#
# Supported: Debian 13, Fedora 43 and later, RHEL, Rocky Linux and
# AlmaLinux 10, and Arch Linux, on x86-64. On Arch, OVN is built from source
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
#   --no-proxy            no HTTPS proxy: the UI stays on 127.0.0.1:8080
#   --external-cidr CIDR  the external network VMs reach the outside through
#                         (default 10.250.250.0/24, as install-agent.sh NATs
#                         it on each host), or "none"
#   --dns IP[,IP...]      the name servers VMs get by DHCP (default: this
#                         machine's, unless they are on loopback), or "none"
#   --no-image            do not add the Debian 13 cloud image
#   --version VERSION     metastratum release to install (default: the latest)
#   --packages DIR        install metastratum from the packages in DIR (as
#                         built by deploy/packaging/build.sh) instead
#   -y, --yes             do not ask before changing the system
#
# The administrator's password: MST_ADMIN_PASSWORD, or asked for; without a
# terminal, made up and kept in /root/metastratum-admin-password.
set -euo pipefail

ROLE=controller
API=http://127.0.0.1:8080
IMAGE_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2

address="" domain="" proxy=1 ext_cidr=10.250.250.0/24 dns="" image=1 version="" packages="" assume_yes=0

while [[ $# -gt 0 ]]; do
  case $1 in
    --address) address=${2:?}; shift ;;
    --domain) domain=${2:?}; shift ;;
    --no-proxy) proxy=0 ;;
    --external-cidr) ext_cidr=${2:?}; shift ;;
    --dns) dns=${2:?}; shift ;;
    --no-image) image=0 ;;
    --version) version=${2#v}; shift ;;
    --packages) packages=$(cd "${2:?}" && pwd); shift ;;
    -y | --yes) assume_yes=1 ;;
    -h | --help) sed -n '/^# Usage:/,/^# terminal/s/^# \{0,1\}//p' "$0"; exit 0 ;;
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

cat <<EOF
This installs a metastratum controller on $host ($PRETTY_NAME):

  - from the distribution: PostgreSQL, OVN's central databases$( ((proxy)) && printf ', Caddy')$([[ $family == arch ]] && printf '\n    (after a full system upgrade; OVN built from source)')
  - metastratum-controller ${version:-(the latest release)}$([[ -n $packages ]] && printf ' from %s' "$packages")
  - the web UI at $(if ((proxy)); then printf 'https://%s' "${domain:-$address}"; else printf 'http://127.0.0.1:8080'; fi)
  - hosts reach it at $address (ports 8443 and 6642)
  - VMs get ${dns:-no} name server(s) by DHCP

EOF
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
  log "installing PostgreSQL and OVN's central databases$( ((proxy)) && printf ' and Caddy')"
  local caddy=()
  ((proxy)) && caddy=(caddy)
  case $family in
    debian)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -q
      apt-get install -y -q postgresql ovn-central curl jq openssl ca-certificates "${caddy[@]}"
      ;;
    fedora) dnf install -y -q postgresql-server postgresql-contrib ovn-central openvswitch curl jq openssl "${caddy[@]}" ;;
    el)
      el_repos
      local ovn ovs
      ovn=$(ovn_pkg central) ovs=$(ovs_pkg)
      [[ -n $ovn && -n $ovs ]] || die "no Open vSwitch or OVN packages in the configured repositories"
      # ovsdb-server, which serves OVN's databases, comes with Open vSwitch.
      dnf install -y -q "$ovn" "$ovs" postgresql-server postgresql-contrib curl jq openssl "${caddy[@]}"
      ;;
    arch)
      pacman -Syu --needed --noconfirm postgresql openvswitch curl jq openssl libarchive "${caddy[@]}"
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
      ;;
  esac
}

start_services() {
  log "starting PostgreSQL and OVN's central databases"
  case $family in
    debian) systemctl enable --now -q postgresql ovn-central ;;
    fedora | el)
      [[ -e /var/lib/pgsql/data/PG_VERSION ]] || postgresql-setup --initdb >/dev/null
      # Password logins over TCP from this machine (the default is ident).
      sed -i -E 's/^(host\s+all\s+all\s+(127\.0\.0\.1\/32|::1\/128)\s+)ident$/\1scram-sha-256/' \
        /var/lib/pgsql/data/pg_hba.conf
      systemctl enable --now -q postgresql ovn-northd
      ;;
    arch)
      if [[ ! -e /var/lib/postgres/data/PG_VERSION ]]; then
        (cd /tmp && runuser -u postgres -- initdb -D /var/lib/postgres/data -E UTF8 \
          --auth-local=peer --auth-host=scram-sha-256 >/dev/null)
      fi
      systemctl enable --now -q postgresql ovn-ovsdb-server-nb ovn-ovsdb-server-sb ovn-northd
      ;;
  esac
  systemctl restart postgresql
}

setup_database() {
  [[ -e $CONF/controller.toml ]] && return
  log "creating the database"
  db_password=$(random_hex 16)
  local verb=CREATE
  [[ $(cd /tmp && runuser -u postgres -- psql -qtAc "SELECT 1 FROM pg_roles WHERE rolname = 'metastratum'") == 1 ]] && verb=ALTER
  # Through stdin: the password stays off command lines.
  (cd /tmp && runuser -u postgres -- psql -q -v ON_ERROR_STOP=1) <<<"$verb ROLE metastratum LOGIN PASSWORD '$db_password';"
  if [[ -z $(cd /tmp && runuser -u postgres -- psql -qtAc "SELECT 1 FROM pg_database WHERE datname = 'metastratum'") ]]; then
    (cd /tmp && runuser -u postgres -- createdb -O metastratum metastratum)
  fi
}

setup_controller() {
  install -d -m 755 "$CONF"
  if [[ ! -e $CONF/controller.toml ]]; then
    log "configuring the controller"
    (
      umask 077
      mst-controller gen-master-key >"$CONF/master.key"
      cat >"$CONF/controller.toml" <<EOF
# metastratum controller, set up by install-controller.sh; each setting is
# explained in /usr/share/doc/metastratum-controller/examples/controller.toml.
database_url = "postgres://metastratum:$db_password@localhost/metastratum"

[api]
# The web UI and API, behind the HTTPS proxy on this machine.
listen = "127.0.0.1:8080"
trusted_proxies = ["127.0.0.1"]

[agents]
listen = "0.0.0.0:8443"
peer_address = "$address:8443"

[ovn]
northbound = ["ssl:$address:6641"]
southbound = ["ssl:$address:6642"]

[network]
# Name servers VMs get by DHCP.
dns_servers = [$(printf '%s' "$dns" | sed 's/[^,][^,]*/"&"/g; s/,/, /g')]
EOF
    )
  fi
  if [[ ! -e /etc/ovn/metastratum/nb.pem ]]; then
    log "serving OVN's databases over TLS"
    local dir
    dir=$(mktemp -d)
    mst-controller ovn-certificate --name "$host" --address "$address,127.0.0.1" \
      --out-dir "$dir" >/dev/null
    sh "$dir/setup.sh" >/dev/null
    rm -rf -- "$dir"
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
  if systemctl -q is-active firewalld; then
    # Hosts' agents, and their ovn-controller.
    firewall-cmd -q --permanent --add-port=8443/tcp --add-port=6642/tcp
    firewall-cmd -q --reload
  fi
}

setup_proxy() {
  ((proxy)) || return 0
  local site=${domain:-$address} file=/etc/caddy/Caddyfile
  if [[ -e $file ]] && ! grep -q 'metastratum' "$file"; then
    # The distributions' Caddyfile serves a placeholder page; anything else
    # is someone's, and stays.
    if grep -qE '^\s*(root|file_server)' "$file" && ! grep -qE '^\s*reverse_proxy' "$file"; then
      mv "$file" "$file.orig"
    else
      warn "$file is in use: add this site to it yourself:
  https://$site { reverse_proxy 127.0.0.1:8080 }"
      return
    fi
  fi
  log "serving the web UI at https://$site"
  install -d /etc/caddy
  {
    printf '# metastratum'"'"'s web UI and API, written by install-controller.sh.\n'
    printf '{\n\tskip_install_trust\n}\n\nhttps://%s {\n' "$site"
    [[ -z $domain ]] && printf '\t# No name to get a certificate for: one from Caddy'"'"'s own CA.\n\ttls internal\n'
    printf '\treverse_proxy 127.0.0.1:8080\n}\n'
  } >"$file"
  if command -v getenforce >/dev/null && [[ $(getenforce) == Enforcing ]]; then
    setsebool -P httpd_can_network_connect 1
  fi
  systemctl enable -q caddy
  systemctl restart caddy
  if systemctl -q is-active firewalld; then
    firewall-cmd -q --permanent --add-service=https --add-service=http
    firewall-cmd -q --reload
  fi
}

api() { # api METHOD PATH [JSON]
  curl -fsS -b "$jar" -X "$1" -H 'content-type: application/json' ${3:+--data "$3"} "$API/api/v1$2"
}

# Through the API: the external network the hosts NAT, the provider's
# default VPC on it, and an image.
provision() {
  [[ -e $STATE/provisioned ]] && return
  if [[ -z $admin_password ]]; then
    [[ -r $admin_password_file ]] || {
      warn "the external network and the image were not set up: run again with MST_ADMIN_PASSWORD set"
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
  if ((image)) && ! api GET /images | jq -e '.[] | select(.name == "debian-13")' >/dev/null; then
    log "adding the Debian 13 cloud image"
    api POST /images "$(jq -n --arg u "$IMAGE_URL" \
      '{name: "debian-13", source_url: $u, format: "qcow2", shared: true}')" >/dev/null
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

  Web UI:  $(if ((proxy)); then printf 'https://%s' "${domain:-$address}"; else printf 'http://127.0.0.1:8080 (forward it: ssh -L 8080:127.0.0.1:8080 root@%s)' "$address"; fi)
  Sign in: admin$([[ -e $admin_password_file ]] && printf ', with the password in %s' "$admin_password_file")
EOF
if ((proxy)) && [[ -z $domain ]]; then
  ca=$(find /var/lib/caddy -path '*authorities/local/root.crt' 2>/dev/null | sed -n 1p)
  cat <<EOF
  The UI's certificate is from Caddy's own CA; browsers warn about it unless
  they trust that CA: ${ca:-/var/lib/caddy/.../pki/authorities/local/root.crt}
EOF
fi
cat <<EOF

Next, hosts: run install-agent.sh on each. On this machine it enrolls with
this controller by itself; elsewhere give it a join token from here
(mst-controller join-token --cluster default, or Infrastructure → Hosts):

  MST_JOIN_TOKEN=... install-agent.sh --controller https://$address:8443

Keep copies of $CONF/controller.toml and master.key somewhere safe.
Check the installation with: mst-controller doctor
EOF
