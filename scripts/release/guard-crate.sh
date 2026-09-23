#!/usr/bin/env bash
# guard-crate.sh <name> <version>: proceed only if that version is not on
# crates.io yet (Linux packages spec §7.4, the crates.io half of
# guard-release.sh). Exit 0 absent, 1 already there, 2 the question could
# not be answered.
#
# A publish is the one step of a release that cannot be undone: yanking
# hides a version but never frees its number, and the bytes behind it can
# never be replaced. So an unanswerable question is exit 2 — never a
# hopeful 0 — and a yanked version counts as taken, exactly as a live one
# does.
set -uo pipefail
if [ $# -ne 2 ]; then
  echo "usage: guard-crate.sh <name> <version>" >&2
  exit 2
fi
name=$1
version=$2
body=$(mktemp)
trap 'rm -f "$body"' EXIT

# crates.io rejects a request with no User-Agent, so this names the caller
# and where to complain about it.
ua="User-Agent: tenuto-release-guard (+https://github.com/alvytsk/tenuto)"
if ! code=$(curl -sS --max-time 30 -H "$ua" -o "$body" -w '%{http_code}' \
  "https://crates.io/api/v1/crates/$name/$version"); then
  echo "guard-crate: could not reach crates.io" >&2
  exit 2
fi

case "$code" in
  404)
    echo "guard-crate: $name $version is not on crates.io yet; proceeding"
    exit 0 ;;
  200)
    echo "guard-crate: $name $version is already on crates.io; a published version is never replaced" >&2
    exit 1 ;;
  *)
    echo "guard-crate: crates.io answered HTTP $code:" >&2
    cat "$body" >&2
    exit 2 ;;
esac
