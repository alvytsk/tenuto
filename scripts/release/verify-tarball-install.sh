#!/usr/bin/env bash
# verify-tarball-install.sh <artifact-dir> <version> <wav> <alsa-package>:
# install only the documented runtime prerequisites, extract the tarball and
# smoke-test it (Linux packages spec §5.1). Run as root in a clean container.
set -euo pipefail
if [ $# -ne 4 ]; then
  echo "usage: verify-tarball-install.sh <artifact-dir> <version> <wav> <alsa-package>" >&2
  exit 2
fi
dir=$1
version=$2
wav=$(realpath "$3")
alsa=$4
here="$(cd "$(dirname "$0")" && pwd)"
export DEBIAN_FRONTEND=noninteractive
fail() { echo "verify-tarball-install: $*" >&2; exit 1; }

cd "$dir"
sha256sum -c SHA256SUMS
apt-get update -qq
apt-get install -y -qq --no-install-recommends "$alsa" ca-certificates
[ -s /etc/ssl/certs/ca-certificates.crt ] || fail "/etc/ssl/certs/ca-certificates.crt is missing or empty"

name="tenuto-${version}-x86_64-unknown-linux-gnu"
extract=$(mktemp -d)
tar -xzf "$name.tar.gz" -C "$extract"
bash "$here/smoke-test.sh" "$extract/$name/tenuto" "$version" "$wav"
echo "verify-tarball-install: passed on $(. /etc/os-release && echo "$PRETTY_NAME")"
