#!/usr/bin/env bash
# Builds a Windows Server image for metastratum from an installation ISO: a
# qcow2 disk that sets itself up on each VM's first boot from the VM's
# cloud-init seed, with cloudbase-init (hostname, the Administrator's
# password from the create form, user data). It has the virtio drivers and
# QEMU's guest agent, Remote Desktop and ping open, and the serial console
# (SAC) on. Add it as an image with operating system Windows.
#
# The install runs unattended in a VM of this user's libvirt session (no
# root needed), from the ISO and the virtio-win ISO, and sysprep generalizes
# it: no build account or password stays usable. Takes 20 to 60 minutes.
#
# Usage: deploy/images/windows/build.sh --iso WINDOWS.iso --virtio virtio-win.iso
#            [--index N] [--drivers 2k25] [--disk-gib 40] [--out FILE]
#            [--cloudbase-init CloudbaseInitSetup_x64.msi]
#   --index    edition in the ISO's install image; on Windows Server 2022 and
#              2025 ISOs 1/3 are Standard/Datacenter (Core), 2/4 the same with
#              Desktop Experience (default 4)
#   --drivers  the virtio-win drivers' folder for this Windows: 2k25, 2k22...
#   --disk-gib the image's disk; VMs' disks are at least this large
#   --out      the image (default windows-server.qcow2)
#   --cloudbase-init  the installer to use (default: Cloudbase's current
#              stable, downloaded)
#
# Environment: LIBVIRT_DEFAULT_URI (default qemu:///session), TMPDIR.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLOUDBASE_INIT_URL=https://cloudbase.it/downloads/CloudbaseInitSetup_Stable_x64.msi
export LIBVIRT_DEFAULT_URI=${LIBVIRT_DEFAULT_URI:-qemu:///session}

log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }
die() { printf '\e[1;31merror:\e[0m %s\n' "$*" >&2; exit 1; }

ISO="" VIRTIO="" INDEX=4 DRIVERS=2k25 DISK_GIB=40 OUT=windows-server.qcow2 MSI=""
while [[ $# -gt 0 ]]; do
  case $1 in
    --iso) ISO=$2; shift 2 ;;
    --virtio) VIRTIO=$2; shift 2 ;;
    --index) INDEX=$2; shift 2 ;;
    --drivers) DRIVERS=$2; shift 2 ;;
    --disk-gib) DISK_GIB=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --cloudbase-init) MSI=$2; shift 2 ;;
    *) sed -n '2,29p' "$0"; exit 1 ;;
  esac
done
[[ -f $ISO ]] || die "--iso: no Windows ISO given (or not found)"
[[ -f $VIRTIO ]] || die "--virtio: no virtio-win ISO given (or not found)"
[[ $INDEX =~ ^[0-9]+$ && $DISK_GIB =~ ^[0-9]+$ && $DRIVERS =~ ^[a-z0-9]+$ ]] || die "bad --index, --disk-gib or --drivers"
[[ ! -e $OUT ]] || die "$OUT exists"
for tool in virt-install virsh qemu-img xorrisofs curl; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done
ISO=$(realpath "$ISO") VIRTIO=$(realpath "$VIRTIO")

umask 077
WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/mst-windows.XXXXXX")
NAME=mst-windows-build-$$
cleanup() {
  virsh destroy "$NAME" >/dev/null 2>&1 || true
  virsh undefine --nvram "$NAME" >/dev/null 2>&1 || true
  rm -f "$WORK"/disk.qcow2 "$WORK"/build.iso "$WORK"/CloudbaseInitSetup_x64.msi
  rm -f "$WORK"/build/* && rmdir "$WORK"/build 2>/dev/null || true
  rmdir "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

mkdir "$WORK/build"
if [[ -n $MSI ]]; then
  cp "$MSI" "$WORK/build/CloudbaseInitSetup_x64.msi"
else
  log "downloading cloudbase-init"
  curl -fsSL -o "$WORK/build/CloudbaseInitSetup_x64.msi" "$CLOUDBASE_INIT_URL"
fi

# The build's Administrator password: random, only ever on this disc and
# the build VM (sysprep makes the image forget the build).
PASSWORD="$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)Aa1!"
# The virtio-win disc's letter is not known in advance: every likely one.
paths="" n=0
for letter in D E F G H; do
  for driver in viostor vioscsi NetKVM; do
    n=$((n + 1))
    paths+="<PathAndCredentials wcm:action=\"add\" wcm:keyValue=\"$n\"><Path>$letter:\\$driver\\$DRIVERS\\amd64</Path></PathAndCredentials>"
  done
done
# (Backslashes doubled: sed reads them in the replacement.)
sed -e "s|@INDEX@|$INDEX|g" -e "s|@PASSWORD@|$PASSWORD|g" -e "s|@DRIVER_PATHS@|${paths//\\/\\\\}|" \
  "$HERE/Autounattend.xml" >"$WORK/build/Autounattend.xml"
cp "$HERE/setup.ps1" "$HERE/cloudbase-init.conf" "$HERE/cloudbase-init-unattend.conf" \
  "$HERE/sysprep-unattend.xml" "$WORK/build/"
xorrisofs -quiet -volid MSTBUILD -joliet -rock -o "$WORK/build.iso" "$WORK/build"
rm -f "$WORK/build/Autounattend.xml"

qemu-img create -q -f qcow2 "$WORK/disk.qcow2" "${DISK_GIB}G"
log "installing Windows in $NAME (watch: virt-viewer -c $LIBVIRT_DEFAULT_URI $NAME)"
virt-install --noautoconsole --name "$NAME" --osinfo detect=on,require=off \
  --vcpus 4 --memory 4096 --machine q35 \
  --boot uefi,firmware.feature0.name=secure-boot,firmware.feature0.enabled=no \
  --disk "path=$WORK/disk.qcow2,format=qcow2,bus=virtio,discard=unmap,boot.order=2" \
  --disk "path=$ISO,device=cdrom,bus=sata,boot.order=1,readonly=on" \
  --disk "path=$VIRTIO,device=cdrom,bus=sata,readonly=on" \
  --disk "path=$WORK/build.iso,device=cdrom,bus=sata,readonly=on" \
  --network none --graphics vnc,listen=127.0.0.1 >/dev/null
# The disc's UEFI loader waits for a key before it boots Windows Setup; then
# its boot menu (on Server ISOs) waits too, once a key stopped its count.
for _ in $(seq 40); do
  virsh send-key "$NAME" KEY_SPACE >/dev/null 2>&1 || true
  sleep 0.5
done
sleep 5
virsh send-key "$NAME" KEY_ENTER >/dev/null 2>&1 || true

# Sysprep shuts it down once the image is done.
start=$SECONDS
until [[ $(virsh domstate "$NAME" 2>/dev/null) == "shut off" ]]; do
  ((SECONDS - start < 3 * 3600)) || die "the install did not finish within 3 hours (the VM is $NAME)"
  if (((SECONDS - start) % 300 < 10 && SECONDS - start > 10)); then
    log "still installing ($(((SECONDS - start) / 60)) minutes)"
  fi
  sleep 10
done
log "installed in $(((SECONDS - start) / 60)) minutes; compressing into $OUT"
qemu-img convert -c -O qcow2 "$WORK/disk.qcow2" "$OUT"
chmod 0644 "$OUT"
log "done: $OUT ($(du -h "$OUT" | cut -f1)); add it as an image with operating system Windows"
