#!/usr/bin/env bash
# build-artifacts.sh <sha> <outdir>: build the release binary once and
# package those bytes as a tarball and a .deb (Linux packages spec §5.1,
# §6, §8). Run at the repository root inside ubuntu:22.04.
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: build-artifacts.sh <sha> <outdir>" >&2
  exit 2
fi
sha=$1
out=$2
here="$(cd "$(dirname "$0")" && pwd)"
fail() { echo "build-artifacts: $*" >&2; exit 1; }

[ "$(git rev-parse HEAD)" = "$sha" ] || fail "HEAD is $(git rev-parse HEAD), expected $sha"
mkdir -p "$out"
version=$(cargo metadata --no-deps --format-version 1 --locked \
  | jq -r '.packages[] | select(.name == "tenuto") | .version')
[ -n "$version" ] || fail "no tenuto version in cargo metadata"

# Strip only here; the project's release profile is untouched.
CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked
bin="${CARGO_TARGET_DIR:-target}/release/tenuto"
elf_report=$(bash "$here/check-elf.sh" "$bin")
echo "$elf_report"

deb="$out/tenuto_${version}-1_amd64.deb"
cargo deb --no-build --no-strip --output "$deb"
depends=$(dpkg-deb -f "$deb" Depends)
echo "Depends: $depends"
grep -qE '(^|, )libasound2(t64)? \(>= [^)]+\)' <<<"$depends" || fail "Depends has no versioned ALSA clause: $depends"
grep -qE '(^|, )libc6 \(>= [^)]+\)' <<<"$depends" || fail "Depends has no versioned libc6: $depends"
grep -qE '(^|, )ca-certificates(,|$)' <<<"$depends" || fail "Depends has no ca-certificates: $depends"

name="tenuto-${version}-x86_64-unknown-linux-gnu"
tarball="$out/$name.tar.gz"
stage=$(mktemp -d)
check=$(mktemp -d)
trap 'rm -rf "$stage" "$check"' EXIT
mkdir "$stage/$name"
install -m 0755 "$bin" "$stage/$name/tenuto"
install -m 0644 README.md CHANGELOG.md LICENSE "$stage/$name/"
mtime=$(git show -s --format=%ct "$sha")
tar --format=gnu --sort=name --owner=0 --group=0 --numeric-owner \
  --mode='u=rwX,go=rX' --mtime="@$mtime" -C "$stage" -cf - "$name" \
  | gzip -n >"$tarball"

# The same bytes in both packages.
want=$(sha256sum <"$bin" | cut -d' ' -f1)
dpkg-deb -x "$deb" "$check/deb"
mkdir "$check/tar"
tar -xzf "$tarball" -C "$check/tar"
[ "$(sha256sum <"$check/deb/usr/bin/tenuto" | cut -d' ' -f1)" = "$want" ] || fail "the .deb binary differs from $bin"
[ "$(sha256sum <"$check/tar/$name/tenuto" | cut -d' ' -f1)" = "$want" ] || fail "the tarball binary differs from $bin"

(cd "$out" && sha256sum "$(basename "$tarball")" "$(basename "$deb")" >SHA256SUMS)
{
  echo "source_sha: $sha"
  echo "version: $version"
  echo "rustc: $(rustc --version)"
  echo "cargo-deb: $(cargo deb --version)"
  echo "deb_depends: $depends"
  echo "binary_sha256: $want"
  echo "$elf_report"
} >"$out/build-info.txt"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "version=$version" >>"$GITHUB_OUTPUT"
fi
echo "build-artifacts: wrote $(ls "$out" | tr '\n' ' ')"
