#!/usr/bin/env bash
# check-elf.sh <binary>: the release binary's runtime requirements
# (Linux packages spec §6.1). Fails on a GLIBC version above the ceiling, a
# private or unrecognized GLIBC version, a NEEDED library outside the
# allowlist, or empty/failed tool output. Run it on a binary built in the
# ubuntu:22.04 container: a host-built binary may legitimately exceed 2.35.
set -euo pipefail
if [ $# -ne 1 ]; then
  echo "usage: check-elf.sh <binary>" >&2
  exit 2
fi
bin=$1
# Test-only overrides; the workflows never set these.
max=${CHECK_ELF_GLIBC_MAX:-2.35}
allow=${CHECK_ELF_ALLOW:-"ld-linux-x86-64.so.2 libasound.so.2 libc.so.6 libgcc_s.so.1 libm.so.6"}

fail() { echo "check-elf: $*" >&2; exit 1; }

dynamic=$(readelf -d "$bin") || fail "readelf failed on $bin"
needed=$(sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\]$/\1/p' <<<"$dynamic" | sort -u)
[ -n "$needed" ] || fail "no NEEDED entries in $bin"

symbols=$(objdump -T "$bin") || fail "objdump failed on $bin"
tokens=$(grep -oE 'GLIBC_[A-Za-z0-9_.]+' <<<"$symbols" | sort -u || true)
[ -n "$tokens" ] || fail "no GLIBC_ version tokens in $bin"

versions=()
while IFS= read -r t; do
  if [ "$t" = GLIBC_PRIVATE ]; then
    fail "$bin uses GLIBC_PRIVATE symbols"
  fi
  if ! [[ "$t" =~ ^GLIBC_[0-9]+(\.[0-9]+)+$ ]]; then
    fail "unrecognized version token '$t' in $bin; review it before extending this check"
  fi
  versions+=("${t#GLIBC_}")
done <<<"$tokens"

highest=$(printf '%s\n' "${versions[@]}" | sort -V | tail -n 1)
top=$(printf '%s\n%s\n' "$highest" "$max" | sort -V | tail -n 1)
if [ "$highest" != "$max" ] && [ "$top" = "$highest" ]; then
  fail "GLIBC_$highest exceeds the $max ceiling"
fi

while IFS= read -r lib; do
  case " $allow " in
    *" $lib "*) ;;
    *) fail "NEEDED $lib is not on the allowlist ($allow)" ;;
  esac
done <<<"$needed"

echo "NEEDED: $(tr '\n' ' ' <<<"$needed" | sed 's/ $//')"
echo "GLIBC max: $highest"
