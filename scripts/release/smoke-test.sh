#!/usr/bin/env bash
# smoke-test.sh <tenuto> <expected-version> <wav>: installation and startup
# checks for a release binary (Linux packages spec §9.1). Steps 1-4 run in
# one isolated home that must stay empty; step 5 plays a file through the
# deviceless null output in a second isolated home.
set -euo pipefail
if [ $# -ne 3 ]; then
  echo "usage: smoke-test.sh <tenuto> <expected-version> <wav>" >&2
  exit 2
fi
tenuto=$1
version=$2
wav=$3

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir "$root/home" "$root/play-home" "$root/out"

fail() {
  echo "smoke-test: $*" >&2
  exit 1
}

# in_home <dir> <cmd...>: HOME and all four XDG directories under <dir>;
# PATH stays the caller's.
in_home() {
  local home=$1
  shift
  env -u TENUTO_AUDIO_OUTPUT \
    HOME="$home" \
    XDG_CONFIG_HOME="$home/config" \
    XDG_DATA_HOME="$home/data" \
    XDG_STATE_HOME="$home/state" \
    XDG_CACHE_HOME="$home/cache" \
    "$@"
}

# 1. --version
in_home "$root/home" "$tenuto" --version >"$root/out/version" 2>&1 \
  || fail "step 1: --version exited non-zero: $(cat "$root/out/version")"
[ "$(cat "$root/out/version")" = "tenuto $version" ] \
  || fail "step 1: --version printed '$(cat "$root/out/version")', expected 'tenuto $version'"

# 2. --help
in_home "$root/home" "$tenuto" --help >"$root/out/help" 2>&1 \
  || fail "step 2: --help exited non-zero"
grep -qF "Usage: tenuto" "$root/out/help" \
  || fail "step 2: --help output has no 'Usage: tenuto'"

# 3. feeds: subscription loading only; it never opens state.json.
in_home "$root/home" "$tenuto" feeds >"$root/out/feeds" 2>&1 \
  || fail "step 3: feeds exited non-zero: $(cat "$root/out/feeds")"
[ "$(wc -l <"$root/out/feeds")" -eq 1 ] \
  || fail "step 3: feeds printed $(wc -l <"$root/out/feeds") lines, expected 1"
head -n 1 "$root/out/feeds" | grep -q '^SLUG' \
  || fail "step 3: feeds did not print the SLUG header"

# 4. Nothing was written.
if [ -n "$(find "$root/home" -mindepth 1 -print -quit)" ]; then
  fail "step 4: the isolated home is not empty: $(find "$root/home" -mindepth 1)"
fi

# 5. Play through the deviceless null output.
in_home "$root/play-home" env TENUTO_AUDIO_OUTPUT=null \
  timeout 30 "$tenuto" play "$wav" </dev/null >"$root/out/play" 2>&1 \
  || fail "step 5: play exited non-zero: $(tail -n 5 "$root/out/play")"
grep -qF "[ended]" "$root/out/play" \
  || fail "step 5: play output has no [ended]"
[ -s "$root/play-home/state/tenuto/state.json" ] \
  || fail "step 5: no state.json under the play home's XDG_STATE_HOME"

echo "smoke-test: $tenuto $version passed all five checks"
