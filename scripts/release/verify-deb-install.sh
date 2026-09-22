#!/usr/bin/env bash
# verify-deb-install.sh <artifact-dir> <version> <wav>: install the built
# .deb with apt in a clean container and smoke-test it (Linux packages spec
# §5.1). Run as root. The container must not have ca-certificates yet.
set -euo pipefail
if [ $# -ne 3 ]; then
  echo "usage: verify-deb-install.sh <artifact-dir> <version> <wav>" >&2
  exit 2
fi
dir=$1
version=$2
wav=$(realpath "$3")
here="$(cd "$(dirname "$0")" && pwd)"
export DEBIAN_FRONTEND=noninteractive
fail() { echo "verify-deb-install: $*" >&2; exit 1; }
installed() { [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null)" = "install ok installed" ]; }

cd "$dir"
sha256sum -c SHA256SUMS
if installed ca-certificates; then
  fail "ca-certificates is already installed; the dependency check would prove nothing"
fi
apt-get update -qq
apt-get install -y -qq "./tenuto_${version}-1_amd64.deb"
installed tenuto || fail "tenuto is not 'install ok installed'"
installed ca-certificates || fail "installing tenuto did not install ca-certificates"
[ -s /etc/ssl/certs/ca-certificates.crt ] || fail "/etc/ssl/certs/ca-certificates.crt is missing or empty"

bash "$here/smoke-test.sh" /usr/bin/tenuto "$version" "$wav"

apt-get remove -y -qq tenuto
[ ! -e /usr/bin/tenuto ] || fail "/usr/bin/tenuto still exists after apt-get remove"
echo "verify-deb-install: passed on $(. /etc/os-release && echo "$PRETTY_NAME")"
