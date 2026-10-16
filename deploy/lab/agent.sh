#!/usr/bin/env bash
# Builds mst-agent as a static (musl) binary and installs it on the lab hosts
# as a systemd service. Hosts that are not enrolled yet get a fresh join token.
#
# Usage: deploy/lab/agent.sh [host...]          (default: all three hosts)
#
# Environment:
#   MST_CONTROLLER       agent endpoint the hosts dial (default https://10.77.0.1:8443)
#   DATABASE_URL         controller database, used to issue join tokens
#   MST_MASTER_KEY_FILE  the controller's master key (join tokens use its CA)
set -euo pipefail

LAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LAB_DIR/../.." && pwd)"
LAB="$LAB_DIR/lab.sh"
CONTROLLER=${MST_CONTROLLER:-https://10.77.0.1:8443}
TARGET=x86_64-unknown-linux-musl
HOSTS=("$@")
[[ ${#HOSTS[@]} -gt 0 ]] || HOSTS=(mst-host1 mst-host2 mst-host3)

log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }

main() {
  log "building a static mst-agent"
  # A static binary runs on any x86-64 Linux host, whatever its libc.
  (cd "$ROOT" && cargo build --release --locked -p mst-agent --target "$TARGET")
  local bin="$ROOT/target/$TARGET/release/mst-agent"

  local host token
  for host in "${HOSTS[@]}"; do
    log "installing on $host"
    "$LAB" ssh "$host" 'cat > /tmp/mst-agent' <"$bin"
    "$LAB" ssh "$host" 'cat > /tmp/mst-agent.service' <"$ROOT/deploy/systemd/mst-agent.service"
    token=""
    if ! "$LAB" ssh "$host" test -e /var/lib/metastratum-agent/host_id; then
      : "${DATABASE_URL:?DATABASE_URL is needed to issue a join token for $host}"
      token=$("$ROOT/target/debug/mst-controller" join-token --ttl-minutes 15)
    fi
    "$LAB" ssh "$host" sudo bash -s -- "$CONTROLLER" "$token" <<'REMOTE'
set -euo pipefail
install -m 0755 /tmp/mst-agent /usr/local/bin/mst-agent
install -m 0644 /tmp/mst-agent.service /etc/systemd/system/mst-agent.service
install -d -m 0755 /etc/metastratum
{
  echo "MST_CONTROLLER=$1"
  # The token is single-use; it is only needed until the host is enrolled.
  # (ssh drops an empty argument, so it may be missing entirely.)
  [[ -n ${2:-} ]] && echo "MST_JOIN_TOKEN=$2"
} > /etc/metastratum/agent.env
chmod 0600 /etc/metastratum/agent.env
systemctl daemon-reload
systemctl enable -q mst-agent
systemctl restart mst-agent

# Live migration needs no libvirt listener: the controller drives it on each
# host's own libvirt, and QEMU sends the VM over TLS with the host
# certificate the agent installs. The agent opens libvirt's TLS listener
# only while a host in its cluster runs an older agent that migrates peer to
# peer, and closes it otherwise.
REMOTE
  done
  log "done; follow logs with: $LAB ssh mst-host1 sudo journalctl -fu mst-agent"
}

main "$@"; exit
