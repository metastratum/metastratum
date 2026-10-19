#!/usr/bin/env bash
# Nested-KVM development lab for metastratum.
#
#   mst-ctl    10.77.0.10   Postgres, OVN central (NB/SB + northd), NFS export,
#                           single-node Ceph (cephadm) for RBD
#   mst-host1  10.77.0.11   libvirt/QEMU, OVS + ovn-controller   (nested KVM)
#   mst-host2  10.77.0.12   〃
#   mst-host3  10.77.0.13   〃
#
# Networks:
#   mst-lab  10.77.0.0/24  management + Geneve underlay (NAT to the internet)
#   mst-ext  10.78.0.0/24  "external" provider network for floating IPs (NAT)
#
# Usage: deploy/lab/lab.sh up | down | status | ceph | ovn-tls | testdb | ssh <vm> [cmd...] | ip <vm>
#
#   ovn-tls serves the OVN databases over TLS instead (northbound on
#           ssl:10.77.0.10:6641, southbound with OVN's RBAC on
#           ssl:10.77.0.10:6642), with certificates from the controller
#   testdb  (re)starts empty, unused OVN northbound databases on mst-ctl for
#           integration tests (plain, TLS and a three-server cluster) and
#           prints the variables that point the tests at them
#
# Requirements: libvirt system daemon (virtqemud) running, the invoking user
# in the `libvirt` group, nested virtualisation enabled, genisoimage, curl.
# Disk images live in a dedicated libvirt storage pool (not under $HOME) so the
# qemu user can read them without sudo.
set -euo pipefail

LAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$LAB_DIR/.state"
export LIBVIRT_DEFAULT_URI=qemu:///system
POOL=mst-lab
POOL_PATH=/var/lib/libvirt/images/mst-lab
BASE_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2
BASE_VOL=debian-13-base.qcow2
SSH_KEY="$STATE/id_ed25519"

# name ip mac vcpus mem_mib disk_gib role
VMS=(
  "mst-ctl   10.77.0.10 52:54:00:77:00:10 2 7168  20 ctl"
  "mst-host1 10.77.0.11 52:54:00:77:00:11 4 6144  80 host"
  "mst-host2 10.77.0.12 52:54:00:77:00:12 4 6144  80 host"
  "mst-host3 10.77.0.13 52:54:00:77:00:13 4 6144  80 host"
)

log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }
die() { printf '\e[1;31merror:\e[0m %s\n' "$*" >&2; exit 1; }

vm_field() { # vm_field <name> <index>
  local line
  for line in "${VMS[@]}"; do
    read -r -a f <<<"$line"
    [[ ${f[0]} == "$1" ]] && { echo "${f[$2]}"; return; }
  done
  die "unknown vm: $1"
}

preflight() {
  virsh uri >/dev/null 2>&1 || die "cannot reach $LIBVIRT_DEFAULT_URI; start it with: sudo systemctl enable --now virtqemud.socket virtnetworkd.socket virtstoraged.socket"
  for t in genisoimage curl virt-install ssh-keygen; do
    command -v "$t" >/dev/null || die "missing tool: $t"
  done
  local nested
  nested=$(cat /sys/module/kvm_intel/parameters/nested 2>/dev/null || cat /sys/module/kvm_amd/parameters/nested 2>/dev/null || echo N)
  [[ $nested == 1 || $nested == Y ]] || die "nested virtualisation is disabled (see docs/development.md: The lab > Setting up your machine, once)"
  virsh domcapabilities --virttype kvm 2>/dev/null | grep -q "<value>efi</value>" ||
    die "libvirt has no UEFI firmware for KVM guests; install edk2-ovmf (Debian/Ubuntu: ovmf)"
  if systemctl is-active -q docker 2>/dev/null; then
    printf '\e[1;33mwarning:\e[0m Docker is running; unless libvirt bridges are allowed in DOCKER-USER, VMs will have no internet (see docs/development.md: The lab > Setting up your machine, once, step 5)\n' >&2
  fi
}

ensure_network() { # ensure_network <name> <prefix> [dhcp-hosts-xml]
  local name=$1 prefix=$2 hosts=${3:-}
  virsh net-info "$name" >/dev/null 2>&1 && return
  log "creating network $name"
  local xml="$STATE/net-$name.xml"
  cat >"$xml" <<EOF
<network>
  <name>$name</name>
  <forward mode='nat'/>
  <bridge stp='off'/>
  <mtu size='9000'/>
  <ip address='$prefix.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='$prefix.100' end='$prefix.199'/>
      $hosts
    </dhcp>
  </ip>
</network>
EOF
  virsh net-define "$xml" >/dev/null
  virsh net-autostart "$name" >/dev/null
  virsh net-start "$name" >/dev/null
}

ensure_pool() {
  virsh pool-info "$POOL" >/dev/null 2>&1 && return
  log "creating storage pool $POOL at $POOL_PATH"
  virsh pool-define-as "$POOL" dir --target "$POOL_PATH" >/dev/null
  virsh pool-build "$POOL" >/dev/null
  virsh pool-autostart "$POOL" >/dev/null
  virsh pool-start "$POOL" >/dev/null
}

ensure_base_image() {
  virsh vol-info --pool "$POOL" "$BASE_VOL" >/dev/null 2>&1 && return
  local img="$STATE/$BASE_VOL"
  if [[ ! -f $img ]]; then
    log "downloading Debian 13 cloud image"
    curl -fL --progress-bar -o "$img.part" "$BASE_URL"
    mv "$img.part" "$img"
  fi
  log "uploading base image to pool"
  virsh vol-create-as "$POOL" "$BASE_VOL" "$(stat -c %s "$img")" --format qcow2 >/dev/null
  virsh vol-upload --pool "$POOL" "$BASE_VOL" "$img"
}

user_data() { # user_data <name> <ip> <role> <ext_mac>
  local name=$1 ip=$2 role=$3 ext_mac=$4 pubkey
  pubkey=$(cat "$SSH_KEY.pub")
  cat <<EOF
#cloud-config
hostname: $name
manage_etc_hosts: true
users:
  - name: mst
    groups: [sudo]
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    ssh_authorized_keys: ["$pubkey"]
write_files:
  - path: /etc/hosts
    append: true
    content: |
      10.77.0.10 mst-ctl
      10.77.0.11 mst-host1
      10.77.0.12 mst-host2
      10.77.0.13 mst-host3
package_update: true
EOF
  if [[ $role == ctl ]]; then
    cat <<'EOF'
packages: [postgresql, ovn-central, nfs-kernel-server, qemu-utils]
runcmd:
  - [sh, -c, "sudo -u postgres psql -c \"CREATE ROLE metastratum LOGIN PASSWORD 'metastratum'\""]
  - [sh, -c, "sudo -u postgres createdb -O metastratum metastratum"]
  - [sh, -c, "pg=$(ls -d /etc/postgresql/*/main); echo \"listen_addresses = '*'\" >> $pg/postgresql.conf; echo 'host all all 10.77.0.0/24 scram-sha-256' >> $pg/pg_hba.conf; systemctl restart postgresql"]
  - [ovn-nbctl, set-connection, "ptcp:6641"]
  - [ovn-sbctl, set-connection, "ptcp:6642"]
  - [mkdir, -p, /srv/nfs/metastratum, /srv/nfs/backups]
  - [sh, -c, "printf '%s\\n' '/srv/nfs/metastratum 10.77.0.0/24(rw,sync,no_subtree_check,no_root_squash)' '/srv/nfs/backups 10.77.0.0/24(rw,sync,no_subtree_check,no_root_squash)' > /etc/exports && exportfs -ra"]
EOF
  else
    cat <<EOF
packages: [qemu-system-x86, qemu-utils, libvirt-daemon-system, openvswitch-switch, ovn-host, nfs-common, genisoimage, ovmf]
runcmd:
  - [usermod, -aG, libvirt, mst]
  # OVS resets system-id from this file at every start; keep the chassis name
  # stable across reboots.
  - [sh, -c, "echo $name > /etc/openvswitch/system-id.conf"]
  - [ovs-vsctl, set, open_vswitch, ., "external_ids:system-id=$name", "external_ids:ovn-remote=tcp:10.77.0.10:6642", "external_ids:ovn-encap-type=geneve", "external_ids:ovn-encap-ip=$ip", "external_ids:ovn-bridge-mappings=external:br-ext"]
  - [ovs-vsctl, --may-exist, add-br, br-ext]
  - [sh, -c, "nic=\$(ip -o link | awk -F': ' '/$ext_mac/ {print \$2}'); ip link set \$nic up && ovs-vsctl --may-exist add-port br-ext \$nic"]
  # Keep the external uplink up across reboots (it carries no address).
  - [sh, -c, "printf '[Match]\\nMACAddress=$ext_mac\\n[Link]\\nActivationPolicy=up\\nRequiredForOnline=no\\n[Network]\\nLinkLocalAddressing=no\\n' > /etc/systemd/network/10-ext.network && networkctl reload"]
  - [systemctl, restart, ovn-controller]
EOF
  fi
}

create_vm() { # create_vm <line>
  read -r name ip mac vcpus mem disk role <<<"$1"
  if virsh dominfo "$name" >/dev/null 2>&1; then
    log "$name already exists"
    return
  fi
  log "creating $name ($role, $ip)"
  local vol="$name.qcow2" seed="$name-seed.iso" dir="$STATE/$name"
  mkdir -p "$dir"
  rm -f "$dir/upgraded"
  local ext_mac=${mac/52:54:00:77/52:54:00:78}
  user_data "$name" "$ip" "$role" "$ext_mac" >"$dir/user-data"
  printf 'instance-id: %s\nlocal-hostname: %s\n' "$name" "$name" >"$dir/meta-data"
  genisoimage -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock "$dir/user-data" "$dir/meta-data"

  virsh vol-delete --pool "$POOL" "$vol" >/dev/null 2>&1 || true
  virsh vol-delete --pool "$POOL" "$seed" >/dev/null 2>&1 || true
  virsh vol-create-as "$POOL" "$vol" "${disk}G" --format qcow2 \
    --backing-vol "$BASE_VOL" --backing-vol-format qcow2 >/dev/null
  virsh vol-create-as "$POOL" "$seed" "$(stat -c %s "$dir/seed.iso")" --format raw >/dev/null
  virsh vol-upload --pool "$POOL" "$seed" "$dir/seed.iso"

  local extra=()
  [[ $role == host ]] && extra+=(--network "network=mst-ext,mac=$ext_mac,model=virtio")
  virt-install --noautoconsole --import \
    --name "$name" --vcpus "$vcpus" --memory "$mem" \
    --cpu host-passthrough --osinfo debian13 \
    --boot uefi,firmware.feature0.name=secure-boot,firmware.feature0.enabled=no \
    --disk "vol=$POOL/$vol,bus=virtio" \
    --disk "vol=$POOL/$seed,device=cdrom" \
    --network "network=mst-lab,mac=$mac,model=virtio" \
    "${extra[@]}" \
    --graphics none --console pty,target_type=serial >/dev/null
}

ssh_vm() { # ssh_vm <name> [cmd...]
  local name=$1; shift
  ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -o ConnectTimeout=5 "mst@$(vm_field "$name" 1)" "$@"
}

# The cloud image lags the archive, its kernel included, and Debian's
# 6.12.111 crashes in openvswitch and geneve under OVN load. Every VM made
# therefore starts on current packages, rebooted into a newer kernel; once
# (a marker says so), not again under a lab in use.
upgrade_vm() { # upgrade_vm <name>
  local name=$1 marker="$STATE/$1/upgraded" boot newest running
  [[ -f $marker ]] && return 0
  log "upgrading packages on $name"
  # Waits for apt-daily, which may hold the lock just after boot.
  ssh_vm "$name" 'sudo apt-get update -qq -o DPkg::Lock::Timeout=600 && sudo DEBIAN_FRONTEND=noninteractive apt-get full-upgrade -y -qq -o DPkg::Lock::Timeout=600 -o Dpkg::Options::=--force-confold >/dev/null' ||
    die "upgrading packages on $name failed: $0 ssh $name sudo apt-get full-upgrade"
  newest=$(ssh_vm "$name" 'ls /boot/vmlinuz-* | sort -V | tail -n1') &&
    running=$(ssh_vm "$name" uname -r) &&
    boot=$(ssh_vm "$name" cat /proc/sys/kernel/random/boot_id) ||
    die "cannot tell which kernel $name runs: $0 ssh $name uname -r"
  if [[ $newest == "/boot/vmlinuz-$running" ]]; then
    touch "$marker"
    return 0
  fi
  log "rebooting $name into its new kernel"
  ssh_vm "$name" sudo systemctl reboot >/dev/null 2>&1 || true
  for _ in $(seq 60); do
    sleep 5
    [[ $(ssh_vm "$name" cat /proc/sys/kernel/random/boot_id 2>/dev/null) == "$boot" ]] && continue
    if ssh_vm "$name" true >/dev/null 2>&1; then
      touch "$marker"
      return 0
    fi
  done
  die "$name did not come back after its reboot; check its console: virsh -c $LIBVIRT_DEFAULT_URI console $name"
}

wait_ready() {
  local line name role
  for line in "${VMS[@]}"; do
    read -r name _ _ _ _ _ role <<<"$line"
    log "waiting for $name to boot"
    for _ in $(seq 60); do
      ssh_vm "$name" true >/dev/null 2>&1 && break
      sleep 5
    done
    ssh_vm "$name" true >/dev/null 2>&1 || die "$name is unreachable over ssh; check its console: virsh -c $LIBVIRT_DEFAULT_URI console $name"
    # Package installs need the internet. On hosts running Docker, its
    # iptables FORWARD policy (DROP) silently blocks libvirt's NAT.
    ssh_vm "$name" curl -fsS -m 10 -o /dev/null http://deb.debian.org/debian/ ||
      die "$name has no internet access through NAT. If Docker runs on this host, allow libvirt bridges:
  sudo iptables -I DOCKER-USER -i virbr+ -j ACCEPT
  sudo iptables -I DOCKER-USER -o virbr+ -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
then re-create the lab: $0 down && $0 up"
    log "waiting for cloud-init on $name"
    ssh_vm "$name" cloud-init status --wait >/dev/null 2>&1 ||
      die "cloud-init failed on $name: $0 ssh $name sudo cloud-init status --long"
    upgrade_vm "$name"
    if [[ $role == host ]]; then
      ssh_vm "$name" test -c /dev/kvm || die "$name has no /dev/kvm (nested virtualisation not working)"
      ssh_vm "$name" virsh -c qemu:///system domcapabilities --virttype kvm >/dev/null 2>&1 ||
        die "libvirt on $name cannot use KVM"
    fi
  done
}

cmd_up() {
  preflight
  mkdir -p "$STATE"
  [[ -f $SSH_KEY ]] || ssh-keygen -q -t ed25519 -N '' -C mst-lab -f "$SSH_KEY"
  local hosts="" line
  for line in "${VMS[@]}"; do
    read -r name ip mac _ <<<"$line"
    hosts+="<host mac='$mac' name='$name' ip='$ip'/>"
  done
  ensure_network mst-lab 10.77.0 "$hosts"
  ensure_network mst-ext 10.78.0
  ensure_pool
  ensure_base_image
  for line in "${VMS[@]}"; do create_vm "$line"; done
  wait_ready
  cmd_ceph
  log "lab is up. Its database: postgres://metastratum:metastratum@10.77.0.10/metastratum"
}

CEPH_MEM_MIB=7168
CEPH_DISK=mst-ctl-ceph.qcow2

# Single-node Ceph on mst-ctl for RBD storage pools: one monitor, one manager
# and one OSD on a dedicated disk, replication size 1. Safe to rerun.
cmd_ceph() {
  local cur
  cur=$(virsh dominfo mst-ctl | awk '/Max memory/ {print int($3 / 1024)}')
  if ! virsh domblklist mst-ctl | grep -q "$CEPH_DISK" || ((cur < CEPH_MEM_MIB)); then
    log "giving mst-ctl ${CEPH_MEM_MIB} MiB and a Ceph disk (restarts mst-ctl)"
    virsh vol-info --pool "$POOL" "$CEPH_DISK" >/dev/null 2>&1 ||
      virsh vol-create-as "$POOL" "$CEPH_DISK" 30G --format qcow2 >/dev/null
    virsh domblklist mst-ctl | grep -q "$CEPH_DISK" ||
      virsh attach-disk mst-ctl --source "$POOL_PATH/$CEPH_DISK" --target vdb --driver qemu --subdriver qcow2 --targetbus virtio --config >/dev/null
    virsh setmaxmem mst-ctl "${CEPH_MEM_MIB}M" --config >/dev/null
    virsh setmem mst-ctl "${CEPH_MEM_MIB}M" --config >/dev/null
    virsh shutdown mst-ctl >/dev/null
    for _ in $(seq 60); do [[ $(virsh domstate mst-ctl) == "shut off" ]] && break; sleep 2; done
    virsh start mst-ctl >/dev/null
    for _ in $(seq 60); do ssh_vm mst-ctl true >/dev/null 2>&1 && break; sleep 5; done
  fi

  log "installing Ceph on mst-ctl (first run downloads the Ceph container image)"
  ssh_vm mst-ctl sudo bash -s <<'REMOTE'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
command -v cephadm >/dev/null || { apt-get update -qq; apt-get install -y -qq cephadm podman ceph-common >/dev/null; }
if [ ! -e /etc/ceph/ceph.conf ]; then
  cephadm bootstrap --mon-ip 10.77.0.10 --single-host-defaults --skip-dashboard \
    --skip-monitoring-stack --skip-firewalld --allow-fqdn-hostname >/var/log/metastratum-ceph-bootstrap.log 2>&1
fi
ceph config set global mon_allow_pool_size_one true
ceph config set global osd_pool_default_size 1
if ! ceph osd ls | grep -q .; then
  ceph orch daemon add osd mst-ctl:/dev/vdb >/dev/null
fi
for _ in $(seq 60); do [ "$(ceph osd stat -f json | python3 -c 'import json,sys; print(json.load(sys.stdin)["num_up_osds"])')" -ge 1 ] && break; sleep 5; done
if ! ceph osd pool ls | grep -qx metastratum; then
  ceph osd pool create metastratum 32 >/dev/null
  ceph osd pool set metastratum size 1 --yes-i-really-mean-it >/dev/null
  rbd pool init metastratum
fi
ceph auth get-or-create client.metastratum mon 'profile rbd' osd 'profile rbd pool=metastratum' >/dev/null
REMOTE

  local h
  for h in mst-host1 mst-host2 mst-host3; do
    ssh_vm "$h" 'command -v rbd >/dev/null || { sudo apt-get update -qq; sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ceph-common >/dev/null; }'
  done

  local key
  key=$(ssh_vm mst-ctl sudo ceph auth get-key client.metastratum)
  log "Ceph is up. Storage pool config for the API:"
  printf '  {"monitors": ["10.77.0.10:6789"], "pool": "metastratum", "user": "metastratum", "key": "%s"}\n' "$key"
}

# Bash run on mst-ctl: make_pki <dir> <name>... creates a CA in <dir> and a
# certificate and key per name, signed by it. Like ovs-pki's, the
# certificates name nothing: OVN checks the CA, not names. Keeps what exists.
read -r -d '' MAKE_PKI <<'SH' || true
make_pki() {
  local dir=$1 who; shift
  mkdir -p "$dir" && cd "$dir" && umask 077
  [[ -f ca.pem ]] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -nodes -days 3650 -subj /CN=mst-ovn-ca -keyout ca-key.pem -out ca.pem 2>/dev/null
  for who; do
    [[ -f $who.pem ]] && continue
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj "/CN=$who" -keyout "$who-key.pem" -out "$who.csr" 2>/dev/null
    openssl x509 -req -in "$who.csr" -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
      -days 3650 -extfile <(echo basicConstraints=CA:FALSE) -out "$who.pem" 2>/dev/null
  done
}
SH

# Switches the OVN databases on mst-ctl to TLS, with the bundle the controller
# writes for an OVN central node. Create it first with
#   mst-controller ovn-certificate --name mst-ctl --address 10.77.0.10 \
#     --out-dir deploy/lab/.state/ovn-central
# then give the controllers ssl:10.77.0.10:6641 and ssl:10.77.0.10:6642 as
# their ovn.northbound and ovn.southbound.
cmd_ovn_tls() {
  local dir="$STATE/ovn-central" f
  [[ -x $dir/setup.sh ]] || die "$dir/setup.sh is missing; see the comment above cmd_ovn_tls"
  log "serving the OVN databases over TLS on ssl:10.77.0.10:6641 (northbound) and :6642 (southbound)"
  ssh_vm mst-ctl 'rm -rf /tmp/ovn-central && mkdir -m 700 /tmp/ovn-central'
  for f in "$dir"/*; do
    ssh_vm mst-ctl "umask 077; cat > /tmp/ovn-central/$(basename "$f")" < "$f"
  done
  ssh_vm mst-ctl 'sudo sh /tmp/ovn-central/setup.sh; rm -rf /tmp/ovn-central'
  log 'controller settings: [ovn] northbound = ["ssl:10.77.0.10:6641"], southbound = ["ssl:10.77.0.10:6642"]'
}


cmd_testdb() {
  log "starting scratch OVN northbound databases on 10.77.0.10"
  { echo "$MAKE_PKI"; cat <<'REMOTE'; } | ssh_vm mst-ctl sudo bash -s
set -euo pipefail
for u in mst-test-nb mst-test-nb-ssl mst-test-nb-c1 mst-test-nb-c2 mst-test-nb-c3; do
  systemctl stop $u 2>/dev/null || true
  systemctl reset-failed $u 2>/dev/null || true
done
d=/run/mst-test-nb
rm -rf $d && mkdir -p $d/pki
schema=/usr/share/ovn/ovn-nb.ovsschema
run() { # run <unit> <db> <args...>
  local unit=$1 db=$2; shift 2
  systemd-run --unit $unit --quiet ovsdb-server $db "$@" \
    --unixctl=$d/$unit.ctl --no-chdir --log-file=$d/$unit.log
}

# Standalone, plain TCP.
ovsdb-tool create $d/nb.db $schema
run mst-test-nb $d/nb.db --remote=ptcp:6651:10.77.0.10

# Standalone over TLS.
make_pki $d/pki server client
ovsdb-tool create $d/nb-ssl.db $schema
run mst-test-nb-ssl $d/nb-ssl.db --remote=pssl:6652:10.77.0.10 \
  --private-key=$d/pki/server-key.pem --certificate=$d/pki/server.pem --ca-cert=$d/pki/ca.pem

# A three-server cluster: clients on 6653-6655, Raft on 6663-6665.
ovsdb-tool create-cluster $d/c1.db $schema tcp:10.77.0.10:6663
for i in 2 3; do
  ovsdb-tool join-cluster $d/c$i.db OVN_Northbound tcp:10.77.0.10:666$((i + 2)) tcp:10.77.0.10:6663
done
for i in 1 2 3; do
  run mst-test-nb-c$i $d/c$i.db --remote=ptcp:665$((i + 2)):10.77.0.10
done
REMOTE
  mkdir -p "$STATE/test-nb-pki"
  local f
  for f in ca.pem client.pem client-key.pem; do
    (umask 077; ssh_vm mst-ctl sudo cat "/run/mst-test-nb/pki/$f" > "$STATE/test-nb-pki/$f")
  done
  cat <<EOF
export MST_TEST_OVN_NB_SCRATCH=tcp:10.77.0.10:6651
export MST_TEST_OVN_NB_SSL=ssl:10.77.0.10:6652
export MST_TEST_OVN_NB_PKI=$STATE/test-nb-pki
export MST_TEST_OVN_NB_CLUSTER=tcp:10.77.0.10:6653,tcp:10.77.0.10:6654,tcp:10.77.0.10:6655
EOF
}

cmd_down() {
  local line name
  for line in "${VMS[@]}"; do
    read -r name _ <<<"$line"
    if virsh dominfo "$name" >/dev/null 2>&1; then
      log "removing $name"
      virsh destroy "$name" >/dev/null 2>&1 || true
      virsh undefine "$name" --nvram >/dev/null 2>&1 || virsh undefine "$name" >/dev/null
    fi
    virsh vol-delete --pool "$POOL" "$name.qcow2" >/dev/null 2>&1 || true
    virsh vol-delete --pool "$POOL" "$name-seed.iso" >/dev/null 2>&1 || true
  done
  virsh vol-delete --pool "$POOL" "$CEPH_DISK" >/dev/null 2>&1 || true
  for net in mst-lab mst-ext; do
    virsh net-destroy "$net" >/dev/null 2>&1 || true
    virsh net-undefine "$net" >/dev/null 2>&1 || true
  done
  log "lab removed (base image kept in pool $POOL; delete with: virsh vol-delete --pool $POOL $BASE_VOL)"
}

cmd_status() {
  virsh list --all | grep -E 'Id|---|mst-' || true
  virsh net-list --all | grep -E 'Name|---|mst-' || true
}

main() {
  case ${1:-} in
    up) cmd_up ;;
    down) cmd_down ;;
    status) cmd_status ;;
    ceph) cmd_ceph ;;
    testdb) cmd_testdb ;;
    ovn-tls) cmd_ovn_tls ;;
    ssh) shift; ssh_vm "$@" ;;
    ip) vm_field "$2" 1 ;;
    *) sed -n '2,27p' "$0"; exit 1 ;;
  esac
}

# Bash reads scripts incrementally; parsing everything into main() and exiting
# on the same line keeps an edit made while the lab is building from being
# executed as a stray fragment.
main "$@"; exit
