#!/usr/bin/env bash
# Installs nfpm (for build.sh) into /usr/local/bin, checked against the
# checksum pinned here. Used by CI; fine on a workstation too.
set -euo pipefail
version=2.48.0
sha256=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750
tarball="nfpm_${version}_Linux_x86_64.tar.gz"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl -fsSL -o "$dir/$tarball" "https://github.com/goreleaser/nfpm/releases/download/v$version/$tarball"
echo "$sha256  $dir/$tarball" | sha256sum -c --quiet -
tar -xzf "$dir/$tarball" -C "$dir" nfpm
sudo install -m 0755 "$dir/nfpm" /usr/local/bin/nfpm
nfpm --version | tail -1
