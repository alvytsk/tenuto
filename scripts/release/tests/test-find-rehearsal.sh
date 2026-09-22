source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/find-rehearsal.sh"
export PATH="$FIXTURES/bin:$PATH"
export GITHUB_REPOSITORY=alvytsk/tenuto
sha=abcdefabcdefabcdefabcdefabcdefabcdefabcd

STUB_GH_RUNS="$FIXTURES/runs/match.json" expect_exit 0 bash "$s" "$sha"
expect_equal "https://example.invalid/runs/2" "$OUT" "matched run url"

for f in wrong-workflow push-event other-branch other-sha failure empty; do
  STUB_GH_RUNS="$FIXTURES/runs/$f.json" expect_exit 1 bash "$s" "$sha"
  expect_contains "no successful rehearsal" "$ERR" "$f"
done

# Lookup failures are 2, never 0 and never 1.
STUB_GH_RUNS="$FIXTURES/runs/not-json.txt" expect_exit 2 bash "$s" "$sha"
STUB_GH_RUNS_FAIL=1 STUB_GH_RUNS="$FIXTURES/runs/match.json" expect_exit 2 bash "$s" "$sha"
STUB_GH_API_FAIL=1 STUB_GH_RUNS="$FIXTURES/runs/match.json" expect_exit 2 bash "$s" "$sha"
STUB_GH_WORKFLOW_ID="" STUB_GH_RUNS="$FIXTURES/runs/match.json" expect_exit 2 bash "$s" "$sha"
STUB_GH_RUNS="$FIXTURES/runs/match.json" expect_exit 2 bash "$s" abc
STUB_GH_RUNS="$FIXTURES/runs/match.json" GITHUB_REPOSITORY="" expect_exit 2 bash "$s" "$sha"

# The stub rejects the invocation the spec's first draft used.
expect_exit 1 gh workflow view release.yml --json id
expect_contains "unknown flag" "$ERR" "stub rejects gh workflow view --json"

finish
