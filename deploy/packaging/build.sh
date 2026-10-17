#!/usr/bin/env bash
# Builds metastratum's packages, .deb and .rpm for the controller and the
# agent, into target/packages. Both binaries are static (musl), so the
# packages suit any recent distribution.
#
# Needs: cargo with the x86_64-unknown-linux-musl target, npm, and nfpm
# (https://nfpm.goreleaser.com; set NFPM to its path if it is not on PATH).
# Versions come from Cargo.toml; RELEASE=1 builds the release itself, else
# a development version that sorts before it (0.1.0~git202610170102.abc1234).
#
# Usage: build.sh [controller|agent ...] [deb|rpm ...]   (default: all of them)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
NFPM=${NFPM:-nfpm}
command -v "$NFPM" >/dev/null || { echo "nfpm not found; install it or set NFPM" >&2; exit 1; }

packages=() formats=()
for a in "$@"; do
  case $a in
    controller | agent) packages+=("$a") ;;
    deb | rpm) formats+=("$a") ;;
    *) echo "unknown package or format: $a" >&2; exit 1 ;;
  esac
done
[[ ${#packages[@]} -gt 0 ]] || packages=(controller agent)
[[ ${#formats[@]} -gt 0 ]] || formats=(deb rpm)

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
if [[ -z ${RELEASE:-} ]]; then
  version+="~git$(git log -1 --format=%cd --date=format:%Y%m%d%H%M).$(git rev-parse --short HEAD)"
fi

cargo build --release --locked --target x86_64-unknown-linux-musl "${packages[@]/#/--package=mst-}"
if [[ " ${packages[*]} " == *" controller "* ]]; then
  [[ -d web/node_modules ]] || (cd web && npm ci)
  (cd web && npm run build)
fi

out=target/packages
mkdir -p "$out"
for pkg in "${packages[@]}"; do
  for format in "${formats[@]}"; do
    VERSION=$version "$NFPM" package --config "deploy/packaging/$pkg.yaml" --packager "$format" --target "$out/"
  done
done
echo "built ${packages[*]} as ${formats[*]}, version $version, in $out"
