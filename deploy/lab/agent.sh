#!/usr/bin/env bash
# Builds the agent's package (deploy/packaging/build.sh) and installs it on
# the lab hosts, with their configuration. Hosts that are not enrolled yet
# get a fresh join token, for the default cluster.
#
# Usage: deploy/lab/agent.sh [host...]          (default: all three hosts)
#
# Environment:
#   MST_CONTROLLER  agent endpoints the hosts dial, comma-separated
#                   (default https://10.77.0.1:8443)
#   MST_CONFIG      the controller's configuration, used to issue join tokens
#   NFPM            nfpm, to build the package (default: nfpm on PATH)
set -euo pipefail

LAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LAB_DIR/../.." && pwd)"
LAB="$LAB_DIR/lab.sh"
CONTROLLER=${MST_CONTROLLER:-https://10.77.0.1:8443}
HOSTS=("$@")
[[ ${#HOSTS[@]} -gt 0 ]] || HOSTS=(mst-host1 mst-host2 mst-host3)

log() { printf '\e[1;34m==>\e[0m %s\n' "$*"; }

main() {
  log "building the agent's package"
  "$ROOT/deploy/packaging/build.sh" agent deb
  local deb
  deb=$(ls -t "$ROOT"/target/packages/metastratum-agent_*.deb | head -1)

  local host token
  for host in "${HOSTS[@]}"; do
    log "installing on $host"
    "$LAB" ssh "$host" 'cat > /tmp/metastratum-agent.deb' <"$deb"
    token=""
    if ! "$LAB" ssh "$host" test -e /var/lib/metastratum-agent/host_id; then
      : "${MST_CONFIG:?MST_CONFIG must name the controller configuration to issue a join token for $host}"
      token=$("$ROOT/target/debug/mst-controller" join-token --ttl-minutes 15 --cluster default)
    fi
    "$LAB" ssh "$host" sudo bash -s -- "$CONTROLLER" "$token" <<'REMOTE'
set -euo pipefail
install -d -m 0755 /etc/metastratum
{
  printf 'controllers = ["%s"]\n' "${1//,/\", \"}"
  # The token is single-use; it is only needed until the host is enrolled.
  # (ssh drops an empty argument, so it may be missing entirely.)
  [[ -n ${2:-} ]] && printf 'join_token = "%s"\n' "$2"
} > /etc/metastratum/agent.toml
chmod 0600 /etc/metastratum/agent.toml
DEBIAN_FRONTEND=noninteractive apt-get install -q -y --reinstall --allow-downgrades /tmp/metastratum-agent.deb >/dev/null
rm -f /tmp/metastratum-agent.deb
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
