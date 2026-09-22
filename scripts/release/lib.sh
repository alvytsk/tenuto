# Test helpers for scripts/release/tests. Sourced, never executed.
# Release scripts themselves must not depend on this file.

RELEASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$RELEASE_DIR/fixtures"
FAILURES=0
PASSES=0

# expect_exit <code> <cmd...>: run cmd, keep stdout in $OUT and stderr in $ERR.
expect_exit() {
  local want=$1; shift
  local out err code
  out=$(mktemp); err=$(mktemp)
  "$@" >"$out" 2>"$err"; code=$?
  OUT=$(cat "$out"); ERR=$(cat "$err")
  rm -f "$out" "$err"
  if [ "$code" -eq "$want" ]; then
    PASSES=$((PASSES + 1))
  else
    FAILURES=$((FAILURES + 1))
    printf 'FAIL: expected exit %s, got %s: %s\n  stdout: %s\n  stderr: %s\n' \
      "$want" "$code" "$*" "$OUT" "$ERR" >&2
  fi
}

# expect_contains <needle> <haystack> [label]
expect_contains() {
  if [[ "$2" == *"$1"* ]]; then
    PASSES=$((PASSES + 1))
  else
    FAILURES=$((FAILURES + 1))
    printf 'FAIL: %s: expected to contain %q, got %q\n' "${3:-output}" "$1" "$2" >&2
  fi
}

# expect_equal <want> <got> [label]
expect_equal() {
  if [ "$1" = "$2" ]; then
    PASSES=$((PASSES + 1))
  else
    FAILURES=$((FAILURES + 1))
    printf 'FAIL: %s: expected %q, got %q\n' "${3:-value}" "$1" "$2" >&2
  fi
}

finish() {
  printf '%s: %d passed, %d failed\n' "$(basename "$0")" "$PASSES" "$FAILURES"
  [ "$FAILURES" -eq 0 ]
}
