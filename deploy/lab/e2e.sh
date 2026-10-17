#!/usr/bin/env bash
# Runs the end-to-end suites (tests/e2e) and the browser tests (web/e2e)
# against the lab, several at once. Suites that change what the others rely
# on run first, on their own: storage (makes the NFS pool others use),
# limits (throttles every VM of the cluster) and m5 (renews the hosts'
# certificates, reconnecting every agent). The browser tests run last, on
# their own too: they migrate a large VM, which needs room on another host.
#
# Usage: deploy/lab/e2e.sh [-j N] [-n] [--changed [REV]] [suite...]
#   suite      an e2e test file (m1, balloon...) or "browser"; default: all
#   -j N       how many run at once (default 3; more run the lab out of memory)
#   -n         only say which suites would run
#   --changed  the suites what changed since REV (default origin/main),
#              committed or not, can affect: all of them, unless every
#              change is in a part known to matter to only some (or none,
#              as with documentation)
#
# The suites read MST_E2E_* from the environment (see tests/e2e/src/lib.rs).
# Each suite's output goes to a log; those of failed suites are printed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FIRST=(storage limits m5)
LAST=(browser)
# The slowest start first, so that none of them is left running alone at
# the end.
SLOW=(m4 resize backups load_balancers m3 balloon clone snapshots)
JOBS=3
CHANGED=
LIST=
while [[ $# -gt 0 ]]; do
  case $1 in
    -j) JOBS=$2; shift 2 ;;
    -n) LIST=1; shift ;;
    --changed)
      CHANGED=origin/main
      shift
      if [[ $# -gt 0 ]] && git -C "$ROOT" rev-parse -q --verify "$1^{commit}" >/dev/null; then
        CHANGED=$1
        shift
      fi
      ;;
    *) break ;;
  esac
done

all() {
  local f
  for f in "$ROOT"/tests/e2e/tests/*.rs; do basename "$f" .rs; done
  echo browser
}
# The suites a changed file can affect, or "all".
affects() {
  case $1 in
    *.md | docs/* | LICENS* | terraform/* | .github/* | deploy/packaging/* | deploy/lab/e2e.sh) ;;
    web/e2e/* | web/src/* | web/index.html | web/public/*) echo browser ;;
    tests/e2e/tests/*.rs) basename "$1" .rs ;;
    crates/mst-netplan/* | crates/mst-ovsdb/* | crates/mst-controller/src/net.rs | \
      crates/mst-controller/src/port_watch.rs | crates/mst-controller/src/ovn_tls.rs | \
      crates/mst-controller/src/api/vpcs.rs | crates/mst-controller/src/api/security.rs | \
      crates/mst-controller/src/api/floating_ips.rs | crates/mst-controller/src/api/external_networks.rs)
      echo m3 load_balancers ;;
    crates/mst-controller/src/api/load_balancers.rs) echo load_balancers ;;
    crates/mst-controller/src/two_factor/* | crates/mst-controller/src/api/two_factor.rs) echo two_factor ;;
    crates/mst-controller/src/api/oidc.rs) echo oidc domains ;;
    crates/mst-controller/src/api/auth.rs) echo oidc domains two_factor m2 browser ;;
    crates/mst-controller/src/api/tenants.rs | crates/mst-controller/src/api/users.rs | \
      crates/mst-controller/src/api/tokens.rs | crates/mst-authz/*) echo m2 domains browser ;;
    crates/mst-controller/src/api/backups.rs | crates/mst-controller/src/engine/backup.rs | \
      crates/mst-agent/src/backup.rs) echo backups ;;
    crates/mst-controller/src/api/snapshots.rs | crates/mst-controller/src/engine/snapshot.rs | \
      crates/mst-agent/src/snapshot.rs) echo snapshots browser ;;
    crates/mst-controller/src/limits.rs) echo limits ;;
    crates/mst-controller/src/fence.rs | crates/mst-controller/src/engine/recover.rs | \
      crates/mst-controller/src/crl.rs | crates/mst-controller/src/ca.rs) echo m5 ;;
    crates/mst-controller/src/migration.rs | crates/mst-controller/src/engine/migrate.rs) echo m4 balloon hotplug cpu browser ;;
    crates/mst-controller/src/cpu.rs) echo cpu m4 m5 ;;
    crates/mst-storage/*) echo storage clone snapshots backups resize m4 disks ;;
    crates/mst-controller/src/api/disks.rs | crates/mst-controller/src/engine/disk.rs | \
      crates/mst-agent/src/transfer.rs) echo disks browser ;;
    *) echo all ;;
  esac
}

SUITES=("$@")
if [[ -n $CHANGED && ${#SUITES[@]} -eq 0 ]]; then
  picked=$(
    {
      git -C "$ROOT" diff --name-only "$CHANGED"
      git -C "$ROOT" ls-files --others --exclude-standard
    } | sort -u | while read -r f; do affects "$f"; done | tr ' ' '\n' | sort -u
  )
  if grep -qx all <<<"$picked"; then
    mapfile -t SUITES < <(all)
  elif [[ -z $picked ]]; then
    echo "nothing changed that the suites test"
    exit 0
  else
    mapfile -t SUITES <<<"$picked"
  fi
  echo "changed since $CHANGED: ${SUITES[*]}"
fi
[[ ${#SUITES[@]} -gt 0 ]] || mapfile -t SUITES < <(all)
if [[ -n $LIST ]]; then
  echo "${SUITES[*]}"
  exit 0
fi

LOGS=$(mktemp -d "${TMPDIR:-/tmp}/mst-e2e.XXXXXX")
echo "logs in $LOGS"

# Built once, up front; the suites then run the test binaries themselves.
cd "$ROOT"
cargo test -q -p mst-e2e --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.profile.test == true and .executable != null) | "\(.target.name) \(.executable)"' \
    >"$LOGS/binaries"

run() {
  local suite=$1 start status
  start=$(date +%s)
  if [[ $suite == browser ]]; then
    (cd "$ROOT/web" && npx playwright test) >"$LOGS/$suite.log" 2>&1 && status=ok || status=FAILED
  else
    local bin
    bin=$(awk -v s="$suite" '$1 == s {print $2}' "$LOGS/binaries")
    if [[ -z $bin ]]; then
      echo "no such suite" >"$LOGS/$suite.log"
      status=FAILED
    else
      (cd "$ROOT/tests/e2e" && "$bin" --test-threads=1) >"$LOGS/$suite.log" 2>&1 && status=ok || status=FAILED
    fi
  fi
  echo "$status" >"$LOGS/$suite.status"
  printf '%-16s %-6s %4ss\n' "$suite" "$status" $(($(date +%s) - start))
  [[ $status == ok ]]
}
export -f run
export ROOT LOGS

among() { [[ " ${*:2} " == *" $1 "* ]]; }
first=() together=() last=()
for s in "${SLOW[@]}"; do among "$s" "${SUITES[@]}" && together+=("$s"); done
for s in "${SUITES[@]}"; do
  if among "$s" "${FIRST[@]}"; then
    first+=("$s")
  elif among "$s" "${LAST[@]}"; then
    last+=("$s")
  elif ! among "$s" "${SLOW[@]}"; then
    together+=("$s")
  fi
done

begin=$(date +%s)
failed=0
for s in "${first[@]}"; do run "$s" || failed=1; done
if [[ ${#together[@]} -gt 0 ]]; then
  printf '%s\n' "${together[@]}" | xargs -P "$JOBS" -I{} bash -c 'run {}' || failed=1
fi
for s in "${last[@]}"; do run "$s" || failed=1; done
echo "took $(($(date +%s) - begin))s"

if [[ $failed -ne 0 ]]; then
  for s in "${SUITES[@]}"; do
    [[ $(cat "$LOGS/$s.status" 2>/dev/null) == ok ]] && continue
    echo "== $s"
    grep -E 'panicked|^Error|✘|Error:' -A8 "$LOGS/$s.log" | head -60
  done
  exit 1
fi
