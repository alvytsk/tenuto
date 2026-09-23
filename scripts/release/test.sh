#!/usr/bin/env bash
# Runs every scripts/release/tests/test-*.sh in its own bash; fails if any fails.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
status=0
for t in "$here"/tests/test-*.sh; do
  bash "$t" || status=1
done
exit "$status"
