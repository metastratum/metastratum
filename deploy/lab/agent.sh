#!/usr/bin/env bash
# Builds hm-agent as a static (musl) binary and installs it on the lab hosts
# as a systemd service. Hosts that are not enrolled yet get a fresh join token.
#
# Usage: deploy/lab/agent.sh [host...]          (default: all three hosts)
#
# Environment:
#   HM_CONTROLLER  agent endpoint the hosts dial (default https://10.77.0.1:8443)
#   DATABASE_URL        controller database, used to issue join tokens
#   HM_MASTER_KEY_FILE  the controller's master key (join tokens use its CA)
set -euo pipefail

LAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LAB_DIR/../.." && pwd)"
LAB="$LAB_DIR/lab.sh"
CONTROLLER=${HM_CONTROLLER:-https://10.77.0.1:8443}
TARGET=x86_64-unknown-linux-musl
HOSTS=("$@")
[[ ${#HOSTS[@]} -gt 0 ]] || HOSTS=(hm-host1 hm-host2 hm-host3)

log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }

main() {
  log "building a static hm-agent"
  # A static binary runs on any x86-64 Linux host, whatever its libc.
  (cd "$ROOT" && cargo build --release --locked -p hm-agent --target "$TARGET")
  local bin="$ROOT/target/$TARGET/release/hm-agent"

  local host token
  for host in "${HOSTS[@]}"; do
    log "installing on $host"
    "$LAB" ssh "$host" 'cat > /tmp/hm-agent' <"$bin"
    "$LAB" ssh "$host" 'cat > /tmp/hm-agent.service' <"$ROOT/deploy/systemd/hm-agent.service"
    token=""
    if ! "$LAB" ssh "$host" test -e /var/lib/hypermon-agent/host_id; then
      : "${DATABASE_URL:?DATABASE_URL is needed to issue a join token for $host}"
      token=$("$ROOT/target/debug/hm-controller" join-token --ttl-minutes 15)
    fi
    "$LAB" ssh "$host" sudo bash -s -- "$CONTROLLER" "$token" <<'REMOTE'
set -euo pipefail
install -m 0755 /tmp/hm-agent /usr/local/bin/hm-agent
install -m 0644 /tmp/hm-agent.service /etc/systemd/system/hm-agent.service
install -d -m 0755 /etc/hypermon
{
  echo "HM_CONTROLLER=$1"
  # The token is single-use; it is only needed until the host is enrolled.
  # (ssh drops an empty argument, so it may be missing entirely.)
  [[ -n ${2:-} ]] && echo "HM_JOIN_TOKEN=$2"
} > /etc/hypermon/agent.env
chmod 0600 /etc/hypermon/agent.env
systemctl daemon-reload
systemctl enable -q hm-agent
systemctl restart hm-agent

# Live migration: libvirt accepts TLS connections from other hosts, with the
# host certificate the agent installs (once it is enrolled).
for _ in $(seq 60); do [[ -e /etc/pki/libvirt/servercert.pem ]] && break; sleep 1; done
if [[ -e /etc/pki/libvirt/servercert.pem ]]; then
  if systemctl is-enabled -q virtproxyd.socket 2>/dev/null; then unit=virtproxyd; else unit=libvirtd; fi
  if ! systemctl is-active -q "$unit-tls.socket"; then
    # systemd only hands a socket to a daemon it starts, so stop it, and its
    # other sockets so no client wakes it up meanwhile. Running VMs are
    # unaffected.
    sockets=("$unit.socket" "$unit-ro.socket" "$unit-admin.socket")
    systemctl stop "${sockets[@]}" "$unit.service"
    systemctl enable -q --now "$unit-tls.socket"
    systemctl start "${sockets[@]}" "$unit.service"
    # The agent reports migration readiness when it (re)connects.
    systemctl restart hm-agent
  fi
else
  echo "hm-agent did not install its certificate; live migration stays off" >&2
fi
REMOTE
  done
  log "done; follow logs with: $LAB ssh hm-host1 sudo journalctl -fu hm-agent"
}

main "$@"; exit
