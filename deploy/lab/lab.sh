#!/usr/bin/env bash
# Nested-KVM development lab for hypermon.
#
#   hm-ctl    10.77.0.10   Postgres, OVN central (NB/SB + northd), NFS export
#   hm-host1  10.77.0.11   libvirt/QEMU, OVS + ovn-controller   (nested KVM)
#   hm-host2  10.77.0.12   〃
#   hm-host3  10.77.0.13   〃
#
# Networks:
#   hm-lab  10.77.0.0/24  management + Geneve underlay (NAT to the internet)
#   hm-ext  10.78.0.0/24  "external" provider network for floating IPs (NAT)
#
# Usage: deploy/lab/lab.sh up | down | status | ssh <vm> [cmd...] | ip <vm>
#
# Requirements: libvirt system daemon (virtqemud) running, the invoking user
# in the `libvirt` group, nested virtualisation enabled, genisoimage, curl.
# Disk images live in a dedicated libvirt storage pool (not under $HOME) so the
# qemu user can read them without sudo.
set -euo pipefail

LAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$LAB_DIR/.state"
export LIBVIRT_DEFAULT_URI=qemu:///system
POOL=hm-lab
POOL_PATH=/var/lib/libvirt/images/hm-lab
BASE_URL=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2
BASE_VOL=debian-13-base.qcow2
SSH_KEY="$STATE/id_ed25519"

# name ip mac vcpus mem_mib disk_gib role
VMS=(
  "hm-ctl   10.77.0.10 52:54:00:77:00:10 2 3072  20 ctl"
  "hm-host1 10.77.0.11 52:54:00:77:00:11 4 6144  40 host"
  "hm-host2 10.77.0.12 52:54:00:77:00:12 4 6144  40 host"
  "hm-host3 10.77.0.13 52:54:00:77:00:13 4 6144  40 host"
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
  [[ $nested == 1 || $nested == Y ]] || die "nested virtualisation is disabled (see README: Lab > One-time host setup)"
  virsh domcapabilities --virttype kvm 2>/dev/null | grep -q "<value>efi</value>" ||
    die "libvirt has no UEFI firmware for KVM guests; install edk2-ovmf (Debian/Ubuntu: ovmf)"
  if systemctl is-active -q docker 2>/dev/null; then
    printf '\e[1;33mwarning:\e[0m Docker is running; unless libvirt bridges are allowed in DOCKER-USER, VMs will have no internet (see README: Lab > One-time host setup, step 5)\n' >&2
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
  - name: hm
    groups: [sudo]
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    ssh_authorized_keys: ["$pubkey"]
write_files:
  - path: /etc/hosts
    append: true
    content: |
      10.77.0.10 hm-ctl
      10.77.0.11 hm-host1
      10.77.0.12 hm-host2
      10.77.0.13 hm-host3
package_update: true
EOF
  if [[ $role == ctl ]]; then
    cat <<'EOF'
packages: [postgresql, ovn-central, nfs-kernel-server, qemu-utils]
runcmd:
  - [sh, -c, "sudo -u postgres psql -c \"CREATE ROLE hypermon LOGIN PASSWORD 'hypermon'\""]
  - [sh, -c, "sudo -u postgres createdb -O hypermon hypermon"]
  - [sh, -c, "pg=$(ls -d /etc/postgresql/*/main); echo \"listen_addresses = '*'\" >> $pg/postgresql.conf; echo 'host all all 10.77.0.0/24 scram-sha-256' >> $pg/pg_hba.conf; systemctl restart postgresql"]
  - [ovn-nbctl, set-connection, "ptcp:6641"]
  - [ovn-sbctl, set-connection, "ptcp:6642"]
  - [mkdir, -p, /srv/nfs/hypermon]
  - [sh, -c, "echo '/srv/nfs/hypermon 10.77.0.0/24(rw,sync,no_subtree_check,no_root_squash)' > /etc/exports && exportfs -ra"]
EOF
  else
    cat <<EOF
packages: [qemu-system-x86, qemu-utils, libvirt-daemon-system, openvswitch-switch, ovn-host, nfs-common, genisoimage, ovmf]
runcmd:
  - [usermod, -aG, libvirt, hm]
  - [ovs-vsctl, set, open_vswitch, ., "external_ids:system-id=$name", "external_ids:ovn-remote=tcp:10.77.0.10:6642", "external_ids:ovn-encap-type=geneve", "external_ids:ovn-encap-ip=$ip", "external_ids:ovn-bridge-mappings=external:br-ext"]
  - [ovs-vsctl, --may-exist, add-br, br-ext]
  - [sh, -c, "nic=\$(ip -o link | awk -F': ' '/$ext_mac/ {print \$2}'); ip link set \$nic up && ovs-vsctl --may-exist add-port br-ext \$nic"]
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
  [[ $role == host ]] && extra+=(--network "network=hm-ext,mac=$ext_mac,model=virtio")
  virt-install --noautoconsole --import \
    --name "$name" --vcpus "$vcpus" --memory "$mem" \
    --cpu host-passthrough --osinfo debian13 \
    --boot uefi,firmware.feature0.name=secure-boot,firmware.feature0.enabled=no \
    --disk "vol=$POOL/$vol,bus=virtio" \
    --disk "vol=$POOL/$seed,device=cdrom" \
    --network "network=hm-lab,mac=$mac,model=virtio" \
    "${extra[@]}" \
    --graphics none --console pty,target_type=serial >/dev/null
}

ssh_vm() { # ssh_vm <name> [cmd...]
  local name=$1; shift
  ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -o ConnectTimeout=5 "hm@$(vm_field "$name" 1)" "$@"
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
  [[ -f $SSH_KEY ]] || ssh-keygen -q -t ed25519 -N '' -C hm-lab -f "$SSH_KEY"
  local hosts="" line
  for line in "${VMS[@]}"; do
    read -r name ip mac _ <<<"$line"
    hosts+="<host mac='$mac' name='$name' ip='$ip'/>"
  done
  ensure_network hm-lab 10.77.0 "$hosts"
  ensure_network hm-ext 10.78.0
  ensure_pool
  ensure_base_image
  for line in "${VMS[@]}"; do create_vm "$line"; done
  wait_ready
  log "lab is up. DATABASE_URL=postgres://hypermon:hypermon@10.77.0.10/hypermon"
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
  for net in hm-lab hm-ext; do
    virsh net-destroy "$net" >/dev/null 2>&1 || true
    virsh net-undefine "$net" >/dev/null 2>&1 || true
  done
  log "lab removed (base image kept in pool $POOL; delete with: virsh vol-delete --pool $POOL $BASE_VOL)"
}

cmd_status() {
  virsh list --all | grep -E 'Id|---|hm-' || true
  virsh net-list --all | grep -E 'Name|---|hm-' || true
}

case ${1:-} in
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  ssh) shift; ssh_vm "$@" ;;
  ip) vm_field "$2" 1 ;;
  *) sed -n '2,20p' "$0"; exit 1 ;;
esac
