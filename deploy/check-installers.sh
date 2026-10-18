#!/usr/bin/env bash
# Checks the installers: they parse, shellcheck finds nothing (where it is
# installed), and the part they share is the same in both. Each must work
# downloaded on its own, so they carry copies of it; with --sync, the
# controller's copy replaces the agent's.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

shared() { sed -n '/^# ---- shared with /,/^# ---- end of the shared part ----$/p' "$1" | sed 1d; }

if [[ ${1:-} == --sync ]]; then
  block=$(mktemp)
  trap 'rm -f -- "$block"' EXIT
  { echo '# ---- shared with install-controller.sh (deploy/check-installers.sh compares) ----'; shared install-controller.sh; } >"$block"
  awk -v block="$block" '
    /^# ---- shared with / || /^@SHARED@$/ { while ((getline l < block) > 0) print l; skip = !/^@SHARED@$/; next }
    skip && /^# ---- end of the shared part ----$/ { skip = 0; next }
    !skip' install-agent.sh >install-agent.sh.new
  chmod 755 install-agent.sh.new
  mv install-agent.sh.new install-agent.sh
fi

status=0
for f in install-controller.sh install-agent.sh; do
  bash -n "$f" || status=1
done
if ! diff <(shared install-controller.sh) <(shared install-agent.sh) >/dev/null; then
  echo "the shared parts of install-controller.sh and install-agent.sh differ (--sync copies the controller's)" >&2
  status=1
fi
if command -v shellcheck >/dev/null; then
  shellcheck install-controller.sh install-agent.sh || status=1
fi
exit $status
