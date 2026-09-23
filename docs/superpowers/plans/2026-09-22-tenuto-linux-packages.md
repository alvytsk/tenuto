# Linux Packages and GitHub Releases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an x86_64 `.tar.gz` and `.deb` of `tenuto` on Ubuntu 22.04, install those exact files on five distros in CI, and publish them as a GitHub Release when a `v*` tag follows a successful rehearsal.

**Architecture:** All decision and build logic lives in bash scripts under `scripts/release/`, tested by `scripts/release/test.sh` with stubbed `gh`/`objdump`/`readelf`/`tenuto`. A reusable workflow `package.yml` calls the build and install scripts. `ci.yml` calls it on every PR. `release.yml` calls it for the rehearsal (`workflow_dispatch`) and the tag run, and only the tag run's `publish` job writes to GitHub.

**Tech Stack:** bash, jq, GNU tar/gzip, binutils (`readelf`, `objdump`), dpkg/apt, cargo-deb 3.8.0, GitHub Actions (`actions/checkout@v6`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`, `Swatinem/rust-cache@v2`), `gh` CLI, clap's `#[command(version)]`.

**Spec:** `docs/superpowers/specs/2026-09-22-tenuto-linux-packages-design.md`. Read it before starting. Section numbers below (§N) refer to it.

## Global Constraints

- Branch: `feat/linux-packages` (already exists; the spec is committed on it).
- Build baseline: `ubuntu:22.04` container. Pinned Rust 1.98.1 (`rust-toolchain.toml`). Every cargo command uses `--locked`.
- Tested install targets, named exactly this way in docs: **Debian 12 and 13; Ubuntu 22.04, 24.04 and 26.04**, x86_64 only. Never write "12+" or "22.04+".
- ALSA runtime package per distro: `libasound2` on `debian:12` and `ubuntu:22.04`; `libasound2t64` on `debian:13`, `ubuntu:24.04` and `ubuntu:26.04`.
- GLIBC ceiling: `2.35`, compared numerically (`sort -V`). `GLIBC_PRIVATE` and any token not matching `^GLIBC_[0-9]+(\.[0-9]+)+$` fail.
- `NEEDED` allowlist (observed on a host build, 2026-09-22): `ld-linux-x86-64.so.2 libasound.so.2 libc.so.6 libgcc_s.so.1 libm.so.6`. Adding a name is a reviewed change.
- The strip happens only in the packaging build: `CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked`, then `cargo deb --no-build --no-strip`. Do **not** add `[profile.release]` to `Cargo.toml`.
- `.deb` `depends = "$auto, ca-certificates"`.
- Artifact names: `tenuto-<ver>-x86_64-unknown-linux-gnu.tar.gz`, `tenuto_<ver>-1_amd64.deb`, `SHA256SUMS`, `build-info.txt`. CI artifact names: `linux-x86_64-<sha>` (published) and `test-inputs-<sha>` (never published).
- Tar flags: `--format=gnu --sort=name --owner=0 --group=0 --numeric-owner --mode='u=rwX,go=rX' --mtime=@<commit time>`, piped through `gzip -n`. Docs say "normalized archive metadata; reproducible builds are not guaranteed".
- `find-rehearsal.sh` exit codes: `0` found, `1` none found, `2` lookup failed.
- Only `release.yml`'s `publish` job has `contents: write`. `package.yml` is `contents: read`.
- Every script: `#!/usr/bin/env bash`, `set -euo pipefail` (or `set -uo pipefail` where the script maps failures to its own exit codes, as stated per script), a usage line, and messages prefixed with the script name. CI invokes scripts as `bash scripts/release/<name>.sh`, because artifacts do not preserve the executable bit.
- Actions run as root inside the test containers, so no `sudo` there.

**Deviation from the spec's file list (§11), deliberate:** the build steps and the two install checks are scripts (`build-artifacts.sh`, `verify-deb-install.sh`, `verify-tarball-install.sh`), not inline YAML, so they can run locally in Docker against the same containers CI uses. The test-inputs artifact therefore carries `scripts/release/` (not only `smoke-test.sh`) plus `tests/fixtures/sine.wav`. Tests live in `scripts/release/tests/`, and `test.sh` runs them.

## File map

| File | Responsibility |
|---|---|
| `src/cli.rs` | Add `version` to `#[command]` |
| `tests/cli.rs` | `--version` test |
| `scripts/release/lib.sh` | Shared test helpers only (`expect_exit`, `pass`, `fail`, summary). Not used by release scripts |
| `scripts/release/test.sh` | Runs every `scripts/release/tests/test-*.sh`, and exits non-zero if any fail |
| `scripts/release/check-tag.sh` | §7.1 |
| `scripts/release/changelog-section.sh` | §7.3 |
| `scripts/release/find-rehearsal.sh` | §7.2 |
| `scripts/release/guard-release.sh` | §7.4 step 2 |
| `scripts/release/check-elf.sh` | §6.1 |
| `scripts/release/smoke-test.sh` | §9.1 |
| `scripts/release/build-artifacts.sh` | §5.1 `build` job body |
| `scripts/release/verify-deb-install.sh` | §5.1 `install-deb` job body |
| `scripts/release/verify-tarball-install.sh` | §5.1 `install-tarball` job body |
| `scripts/release/fixtures/bin/gh` | Argument-validating `gh` stub |
| `scripts/release/fixtures/bin-elf/{readelf,objdump}` | Tool stubs for `check-elf.sh` failure cases |
| `scripts/release/fixtures/bin-tenuto/tenuto` | `tenuto` stub for `smoke-test.sh` cases |
| `scripts/release/fixtures/runs/*.json` | `gh run list` outputs |
| `scripts/release/fixtures/CHANGELOG.*.md` | Changelog cases |
| `scripts/release/tests/test-*.sh` | One test file per script |
| `.github/workflows/package.yml` | New reusable workflow |
| `.github/workflows/ci.yml` | Rename `package` → `crate`; add `linux-packages`, `release-scripts` |
| `.github/workflows/release.yml` | Rewritten |
| `Cargo.toml` | `[package.metadata.deb]`; `exclude` gains `/scripts` |
| `docs/architecture.md`, `README.md`, `CHANGELOG.md` | §10 |

---

### Task 1: `tenuto --version`

**Files:**
- Modify: `src/cli.rs:8`
- Test: `tests/cli.rs` (append)
- Modify: `CHANGELOG.md` (Unreleased → Added)

**Interfaces:**
- Produces: `tenuto --version` prints `tenuto <CARGO_PKG_VERSION>\n` and exits 0. `smoke-test.sh` (Task 5) depends on this exact format.

- [ ] **Step 1: Write the failing test.** Append to `tests/cli.rs`:

```rust
/// `--version` is what the package smoke test compares against the release
/// tag (Linux packages spec §9.1), so its format is part of the contract:
/// exactly `tenuto <version>` and a newline, exit 0.
#[test]
fn version_flag_prints_the_package_version() {
    let profile = process::Profile::new().unwrap();
    let output = profile.command().arg("--version").output().unwrap();
    assert!(output.status.success(), "--version exits 0");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("tenuto {}\n", env!("CARGO_PKG_VERSION"))
    );
}
```

- [ ] **Step 2: Run it and confirm it fails.**

Run: `cargo test --locked --test cli version_flag_prints_the_package_version`
Expected: FAIL. The assertion `--version exits 0` fails, because clap reports `unexpected argument '--version'` and exits 2.

- [ ] **Step 3: Implement.** In `src/cli.rs`, change line 8 to:

```rust
#[command(name = "tenuto", version, about = "A keyboard-first terminal audio player")]
```

- [ ] **Step 4: Run the test and the whole CLI file.**

Run: `cargo test --locked --test cli`
Expected: all PASS.

- [ ] **Step 5: Changelog.** Under `## [Unreleased]` → `### Added` in `CHANGELOG.md`, add at the end of the list:

```markdown
- `tenuto --version` prints the version.
```

- [ ] **Step 6: Lints.**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit.**

```bash
git add src/cli.rs tests/cli.rs CHANGELOG.md
git commit -m "feat(cli): tenuto --version"
```

---

### Task 2: Test harness, `check-tag.sh` and `changelog-section.sh`

**Files:**
- Create: `scripts/release/lib.sh`, `scripts/release/test.sh`
- Create: `scripts/release/check-tag.sh`, `scripts/release/changelog-section.sh`
- Create: `scripts/release/tests/test-check-tag.sh`, `scripts/release/tests/test-changelog-section.sh`
- Create: `scripts/release/fixtures/CHANGELOG.ok.md`, `CHANGELOG.dup.md`, `CHANGELOG.empty.md`
- Modify: `Cargo.toml` (`exclude` gains `"/scripts"`)

**Interfaces:**
- Produces: `check-tag.sh <tag> <version>` exits 0 on match, 1 on mismatch, 2 on usage error.
- Produces: `changelog-section.sh <version> [changelog-path]` prints the section body to stdout and exits 0. It exits 1 if the section is missing, duplicated or empty, and 2 on a usage error. The default path is `CHANGELOG.md`.
- Produces (tests only): `lib.sh` functions `expect_exit <code> <cmd...>` (runs cmd, captures stdout to `$OUT` and stderr to `$ERR`, records pass/fail), `expect_contains <needle> <haystack>`, `finish` (prints a summary and returns non-zero on any failure). Each test file sources `lib.sh`, and `test.sh` runs each test file in a fresh `bash`.

- [ ] **Step 1: Write the harness.** `scripts/release/lib.sh`:

```bash
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
```

`scripts/release/test.sh`:

```bash
#!/usr/bin/env bash
# Runs every scripts/release/tests/test-*.sh in its own bash; fails if any fails.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
status=0
for t in "$here"/tests/test-*.sh; do
  bash "$t" || status=1
done
exit "$status"
```

- [ ] **Step 2: Write the fixtures.**

`scripts/release/fixtures/CHANGELOG.ok.md`:

```markdown
# Changelog

## [Unreleased]

### Added

- Unreleased thing.

## [0.2.0] - 2026-10-01

### Fixed

- A fix in 0.2.0.

## [0.1.0] - 2026-09-17

### Added

- First release.

[Unreleased]: https://example.invalid/compare/v0.2.0...HEAD
[0.2.0]: https://example.invalid/compare/abc...v0.2.0
[0.1.0]: https://example.invalid/commit/abc
```

`scripts/release/fixtures/CHANGELOG.dup.md`:

```markdown
# Changelog

## [0.2.0] - 2026-10-01

- One.

## [0.2.0] - 2026-10-02

- Two.
```

`scripts/release/fixtures/CHANGELOG.empty.md`:

```markdown
# Changelog

## [0.2.0] - 2026-10-01


## [0.1.0] - 2026-09-17

- First release.
```

- [ ] **Step 3: Write the failing tests.**

`scripts/release/tests/test-check-tag.sh`:

```bash
source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/check-tag.sh"

expect_exit 0 bash "$s" v0.1.3 0.1.3
expect_exit 1 bash "$s" 0.1.3 0.1.3
expect_contains "expected 'v0.1.3'" "$ERR" "missing v message"
expect_exit 1 bash "$s" v0.1.3-rc1 0.1.3
expect_exit 1 bash "$s" v0.1.4 0.1.3
expect_exit 2 bash "$s" v0.1.3

finish
```

`scripts/release/tests/test-changelog-section.sh`:

```bash
source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/changelog-section.sh"

expect_exit 0 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.ok.md"
expect_equal $'### Fixed\n\n- A fix in 0.2.0.' "$OUT" "0.2.0 body"

# The last section stops at the link references, not at EOF.
expect_exit 0 bash "$s" 0.1.0 "$FIXTURES/CHANGELOG.ok.md"
expect_equal $'### Added\n\n- First release.' "$OUT" "0.1.0 body"

expect_exit 1 bash "$s" 0.3.0 "$FIXTURES/CHANGELOG.ok.md"
expect_contains "no section" "$ERR"
expect_exit 1 bash "$s" Unreleased "$FIXTURES/CHANGELOG.ok.md"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.dup.md"
expect_contains "2 sections" "$ERR"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.empty.md"
expect_contains "empty" "$ERR"
# A dot is not a wildcard: 0x2x0 must not match 0.2.0.
expect_exit 1 bash "$s" 0x2x0 "$FIXTURES/CHANGELOG.ok.md"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/does-not-exist.md"
expect_exit 2 bash "$s"

finish
```

- [ ] **Step 4: Run them and confirm they fail.**

Run: `bash scripts/release/test.sh`
Expected: FAIL lines for every case (the scripts don't exist; `bash` exits 127), and a non-zero exit.

- [ ] **Step 5: Implement `check-tag.sh`.**

```bash
#!/usr/bin/env bash
# check-tag.sh <tag> <version>: the tag must be exactly "v" + the Cargo.toml
# version (Linux packages spec §7.1). Exit 0 match, 1 mismatch, 2 usage.
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: check-tag.sh <tag> <version>" >&2
  exit 2
fi
tag=$1
version=$2
if [ "$tag" != "v$version" ]; then
  echo "check-tag: tag '$tag' does not match Cargo.toml version '$version' (expected 'v$version')" >&2
  exit 1
fi
echo "check-tag: $tag matches Cargo.toml $version"
```

- [ ] **Step 6: Implement `changelog-section.sh`.** Heading matching uses `index()` (a literal prefix) plus a date check written without `{n}` intervals, so it behaves the same under mawk and gawk.

```bash
#!/usr/bin/env bash
# changelog-section.sh <version> [changelog]: print the body of
# "## [<version>] - YYYY-MM-DD", up to the next "## " heading or the link
# reference block, without the heading (Linux packages spec §7.3).
# Exit 0 printed, 1 missing/duplicated/empty section, 2 usage.
set -euo pipefail
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: changelog-section.sh <version> [changelog]" >&2
  exit 2
fi
version=$1
file=${2:-CHANGELOG.md}
if [ ! -f "$file" ]; then
  echo "changelog-section: $file does not exist" >&2
  exit 1
fi
heading="## [$version] - "
date_re='^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$'

count=$(awk -v h="$heading" -v re="$date_re" '
  index($0, h) == 1 && substr($0, length(h) + 1) ~ re { n++ }
  END { print n + 0 }' "$file")
if [ "$count" -eq 0 ]; then
  echo "changelog-section: no section '## [$version] - YYYY-MM-DD' in $file" >&2
  exit 1
fi
if [ "$count" -gt 1 ]; then
  echo "changelog-section: $count sections for $version in $file" >&2
  exit 1
fi

body=$(awk -v h="$heading" -v re="$date_re" '
  on && (/^## / || /^\[[^]]+\]: /) { exit }
  on { print }
  index($0, h) == 1 && substr($0, length(h) + 1) ~ re { on = 1 }' "$file" \
  | sed '/./,$!d')
if [ -z "${body//[[:space:]]/}" ]; then
  echo "changelog-section: the $version section in $file is empty" >&2
  exit 1
fi
printf '%s\n' "$body"
```

(`$(...)` drops trailing newlines, and `sed '/./,$!d'` drops leading blank lines, so the body starts and ends on content.)

- [ ] **Step 7: Run the tests.**

Run: `bash scripts/release/test.sh`
Expected: `test-changelog-section.sh: 14 passed, 0 failed`, `test-check-tag.sh: 6 passed, 0 failed`, exit 0.

- [ ] **Step 8: Run both scripts against the real changelog.**

Run: `bash scripts/release/changelog-section.sh 0.1.2`
Expected: exit 0, and the printed body of the 0.1.2 entry, ending before `## [0.1.1]`.

- [ ] **Step 9: Keep scripts out of the crate.** In `Cargo.toml`, change the `exclude` line to:

```toml
exclude      = ["/tests", "/docs", "/.github", "/.claude", "/rust-toolchain.toml", "/scripts"]
```

Run: `cargo package --locked --list | grep -c '^scripts/' || true`
Expected: `0`.

- [ ] **Step 10: Commit.**

```bash
chmod +x scripts/release/*.sh
git add scripts/release Cargo.toml
git commit -m "feat(release): tag and changelog checks, with a shell test harness"
```

---

### Task 3: `find-rehearsal.sh`, `guard-release.sh` and the `gh` stub

**Files:**
- Create: `scripts/release/find-rehearsal.sh`, `scripts/release/guard-release.sh`
- Create: `scripts/release/fixtures/bin/gh`
- Create: `scripts/release/fixtures/runs/{match,wrong-workflow,push-event,other-branch,other-sha,failure,empty}.json`, `scripts/release/fixtures/runs/not-json.txt`
- Create: `scripts/release/tests/test-find-rehearsal.sh`, `scripts/release/tests/test-guard-release.sh`

**Interfaces:**
- Consumes: env `GITHUB_REPOSITORY` (`owner/repo`); `gh` authenticated via `GH_TOKEN`.
- Produces: `find-rehearsal.sh <sha>` exits 0 (prints the run URL), 1 (no qualifying run), or 2 (lookup failed or bad input).
- Produces: `guard-release.sh <tag>` exits 0 only when no release (draft or published) exists for the tag. It exits 1 when one exists and 2 when `gh` fails for another reason.
- Stub contract (`fixtures/bin/gh`): it accepts only these argument vectors, and anything else prints `unknown flag: <args>` to stderr and exits 1.
  1. `api repos/<GITHUB_REPOSITORY>/actions/workflows/release.yml --jq .id`: prints `$STUB_GH_WORKFLOW_ID` (`4242` when unset; an empty value prints an empty line), or exits 1 if `STUB_GH_API_FAIL=1`.
  2. `run list --workflow release.yml --event workflow_dispatch --branch main --commit <40-hex> --status success --limit 200 --json databaseId,headSha,headBranch,event,conclusion,workflowDatabaseId,displayTitle,url`: prints the file `$STUB_GH_RUNS` with `@SHA@` replaced by the `--commit` value, or exits 1 if `STUB_GH_RUNS_FAIL=1`.
  3. `release view <tag> --json isDraft`: `STUB_GH_RELEASE=none` prints `release not found` to stderr and exits 1; `draft` prints `{"isDraft":true}`; `published` prints `{"isDraft":false}`; `error` prints `HTTP 502` to stderr and exits 1.

- [ ] **Step 1: Write the stub.** `scripts/release/fixtures/bin/gh`:

```bash
#!/usr/bin/env bash
# Argument-validating stand-in for gh (Linux packages spec §9.2). It accepts
# only the exact invocations the release scripts are specified to make, so a
# wrong subcommand or flag fails here as it would against the real gh.
set -uo pipefail
args="$*"
sha_re='^[0-9a-f]{40}$'

if [ "$args" = "api repos/${GITHUB_REPOSITORY:-}/actions/workflows/release.yml --jq .id" ]; then
  [ "${STUB_GH_API_FAIL:-0}" = 1 ] && { echo "HTTP 404: Not Found" >&2; exit 1; }
  echo "${STUB_GH_WORKFLOW_ID-4242}"
  exit 0
fi

if [ $# -eq 16 ] && [ "$1 $2 $3 $4 $5 $6 $7 $8" = "run list --workflow release.yml --event workflow_dispatch --branch main" ] \
   && [ "$9" = "--commit" ] && [[ "${10}" =~ $sha_re ]] \
   && [ "${11} ${12} ${13} ${14} ${15} ${16}" = "--status success --limit 200 --json databaseId,headSha,headBranch,event,conclusion,workflowDatabaseId,displayTitle,url" ]; then
  [ "${STUB_GH_RUNS_FAIL:-0}" = 1 ] && { echo "HTTP 502" >&2; exit 1; }
  sed "s/@SHA@/${10}/g" "${STUB_GH_RUNS:?STUB_GH_RUNS not set}"
  exit 0
fi

if [ $# -eq 5 ] && [ "$1 $2" = "release view" ] && [ "$4 $5" = "--json isDraft" ]; then
  case "${STUB_GH_RELEASE:-none}" in
    none) echo "release not found" >&2; exit 1 ;;
    draft) echo '{"isDraft":true}'; exit 0 ;;
    published) echo '{"isDraft":false}'; exit 0 ;;
    error) echo "HTTP 502: Bad Gateway" >&2; exit 1 ;;
  esac
fi

echo "unknown flag: $args" >&2
exit 1
```

The `run list` vector is 16 words: `run list` (2) + the `--workflow`, `--event`, `--branch` pairs (6) + the `--commit` pair (`$9`, `${10}`) + the `--status`, `--limit`, `--json` pairs (`${11}`–`${16}`). `STUB_GH_WORKFLOW_ID` uses `${…-4242}` (no colon), so an explicitly empty value stays empty and the empty-id test can reach the script's check.

Make it executable now, since the tests call it through `PATH`: `chmod +x scripts/release/fixtures/bin/gh`.

- [ ] **Step 2: Write the run fixtures.** Each is a JSON array. `@SHA@` is replaced by the stub.

`runs/match.json`: one qualifying run and one non-qualifying run, so the filter must pick:

```json
[
  {"databaseId": 1, "headSha": "@SHA@", "headBranch": "main", "event": "workflow_dispatch", "conclusion": "failure", "workflowDatabaseId": 4242, "displayTitle": "Release rehearsal @SHA@", "url": "https://example.invalid/runs/1"},
  {"databaseId": 2, "headSha": "@SHA@", "headBranch": "main", "event": "workflow_dispatch", "conclusion": "success", "workflowDatabaseId": 4242, "displayTitle": "Release rehearsal @SHA@", "url": "https://example.invalid/runs/2"}
]
```

For the next five, copy the run with `databaseId` 2 and change exactly one field:
- `runs/wrong-workflow.json`: `"workflowDatabaseId": 9999`
- `runs/push-event.json`: `"event": "push"`
- `runs/other-branch.json`: `"headBranch": "feature"`
- `runs/other-sha.json`: `"headSha": "1111111111111111111111111111111111111111"`
- `runs/failure.json`: `"conclusion": "failure"`

`runs/empty.json`: `[]`
`runs/not-json.txt`: `this is not json`

- [ ] **Step 3: Write the failing tests.**

`scripts/release/tests/test-find-rehearsal.sh`:

```bash
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
```

`scripts/release/tests/test-guard-release.sh`:

```bash
source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/guard-release.sh"
export PATH="$FIXTURES/bin:$PATH"

STUB_GH_RELEASE=none expect_exit 0 bash "$s" v0.1.3
STUB_GH_RELEASE=draft expect_exit 1 bash "$s" v0.1.3
expect_contains "incomplete draft" "$ERR" "draft message"
STUB_GH_RELEASE=published expect_exit 1 bash "$s" v0.1.3
expect_contains "never overwritten" "$ERR" "published message"
STUB_GH_RELEASE=error expect_exit 2 bash "$s" v0.1.3
expect_exit 2 bash "$s"

finish
```

- [ ] **Step 4: Run them and confirm they fail.**

Run: `bash scripts/release/test.sh`
Expected: the two new files report failures, and the earlier files still pass.

- [ ] **Step 5: Implement `find-rehearsal.sh`.** It uses `set -uo pipefail` (no `-e`), because every failure maps to exit 2 explicitly.

```bash
#!/usr/bin/env bash
# find-rehearsal.sh <sha>: is there a successful workflow_dispatch run of
# release.yml on main for exactly this commit? (Linux packages spec §7.2)
# Exit 0 found (prints its URL), 1 none found, 2 the lookup itself failed.
set -uo pipefail
if [ $# -ne 1 ]; then
  echo "usage: find-rehearsal.sh <sha>" >&2
  exit 2
fi
sha=$1
if ! [[ "$sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "find-rehearsal: '$sha' is not a full 40-character SHA" >&2
  exit 2
fi
if [ -z "${GITHUB_REPOSITORY:-}" ]; then
  echo "find-rehearsal: GITHUB_REPOSITORY is not set" >&2
  exit 2
fi

# gh workflow view has no --json flag; the REST API gives the workflow id.
if ! workflow_id=$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/release.yml" --jq .id); then
  echo "find-rehearsal: could not read the release.yml workflow id" >&2
  exit 2
fi
if ! [[ "$workflow_id" =~ ^[0-9]+$ ]]; then
  echo "find-rehearsal: workflow id '$workflow_id' is not a number" >&2
  exit 2
fi

if ! runs=$(gh run list --workflow release.yml --event workflow_dispatch --branch main \
      --commit "$sha" --status success --limit 200 \
      --json databaseId,headSha,headBranch,event,conclusion,workflowDatabaseId,displayTitle,url); then
  echo "find-rehearsal: gh run list failed" >&2
  exit 2
fi

# Re-filter on every field: a server-side filter that stopped working must
# not let a wrong run through. The title is never matched on.
if ! urls=$(jq -er --argjson wid "$workflow_id" --arg sha "$sha" '
    if type != "array" then error("not an array") else . end
    | [ .[] | select(.workflowDatabaseId == $wid
                     and .event == "workflow_dispatch"
                     and .headBranch == "main"
                     and .headSha == $sha
                     and .conclusion == "success") | .url ]
    | if length == 0 then "" else .[0] end' <<<"$runs"); then
  echo "find-rehearsal: gh run list returned something other than a JSON array" >&2
  exit 2
fi

if [ -z "$urls" ]; then
  echo "find-rehearsal: no successful rehearsal of release.yml on main for $sha; dispatch release.yml on main with sha=$sha first" >&2
  exit 1
fi
echo "$urls"
```

Note on `jq -e`: `-e` makes jq exit 1 when the last output is `false` or `null`. An empty string `""` is truthy, so the "none found" path still exits 0 from jq, and non-JSON input or the `error(...)` exits non-zero, giving exit 2. Confirm the `empty.json` case in Step 6 returns 1, not 2.

- [ ] **Step 6: Implement `guard-release.sh`.**

```bash
#!/usr/bin/env bash
# guard-release.sh <tag>: proceed only if no release exists for the tag
# (Linux packages spec §7.4). Exit 0 none, 1 one exists, 2 gh failed.
set -uo pipefail
if [ $# -ne 1 ]; then
  echo "usage: guard-release.sh <tag>" >&2
  exit 2
fi
tag=$1
err=$(mktemp)
trap 'rm -f "$err"' EXIT

if out=$(gh release view "$tag" --json isDraft 2>"$err"); then
  case "$(jq -r .isDraft <<<"$out" 2>/dev/null)" in
    true)
      echo "guard-release: an incomplete draft for $tag exists; inspect and delete it, then rerun" >&2
      exit 1 ;;
    false)
      echo "guard-release: $tag is already published; a published release is never overwritten" >&2
      exit 1 ;;
    *)
      echo "guard-release: unexpected gh output: $out" >&2
      exit 2 ;;
  esac
fi
if grep -q "release not found" "$err"; then
  echo "guard-release: no release for $tag yet; proceeding"
  exit 0
fi
echo "guard-release: gh release view failed:" >&2
cat "$err" >&2
exit 2
```

- [ ] **Step 7: Run the tests.**

Run: `bash scripts/release/test.sh`
Expected: every file reports `0 failed`, exit 0. If `empty.json` gives 2, the `jq` program is wrong: fix it so an empty selection yields `""`.

- [ ] **Step 8: Live probe against the real repository**, with a real token and the real `gh`:

Run: `GITHUB_REPOSITORY=alvytsk/tenuto bash scripts/release/find-rehearsal.sh 0000000000000000000000000000000000000000; echo "exit=$?"`
Expected: `no successful rehearsal …` and `exit=1`. An `exit=2` means a real flag or field is wrong, so fix the script **and** the stub.

- [ ] **Step 9: Commit.**

```bash
chmod +x scripts/release/*.sh scripts/release/fixtures/bin/gh
git add scripts/release
git commit -m "feat(release): rehearsal lookup and existing-release guard, with an argument-checking gh stub"
```

---

### Task 4: `check-elf.sh`

**Files:**
- Create: `scripts/release/check-elf.sh`
- Create: `scripts/release/fixtures/bin-elf/objdump`, `scripts/release/fixtures/bin-elf/readelf`
- Create: `scripts/release/tests/test-check-elf.sh`

**Interfaces:**
- Produces: `check-elf.sh <binary>` exits 0 and prints exactly two lines:
  ```
  NEEDED: <space-separated sorted list>
  GLIBC max: <x.y[.z]>
  ```
  It exits 1 on any violation or tool failure and 2 on a usage error. The test-only env overrides are `CHECK_ELF_ALLOW` (space-separated list) and `CHECK_ELF_GLIBC_MAX`.
- Stub contract (`fixtures/bin-elf/*`): `objdump` prints the contents of `$STUB_OBJDUMP` (a literal string) and exits 0. `readelf` exits 1 if `STUB_READELF_FAIL=1`, and otherwise runs the real readelf (`/usr/bin/readelf "$@"`).

- [ ] **Step 1: Write the stubs.**

`fixtures/bin-elf/objdump`:

```bash
#!/usr/bin/env bash
printf '%s\n' "${STUB_OBJDUMP-}"
```

`fixtures/bin-elf/readelf`:

```bash
#!/usr/bin/env bash
[ "${STUB_READELF_FAIL:-0}" = 1 ] && { echo "readelf: Error: not an ELF file" >&2; exit 1; }
exec /usr/bin/readelf "$@"
```

Run `chmod +x scripts/release/fixtures/bin-elf/*`. The tests call these through `PATH`.

- [ ] **Step 2: Write the failing tests.** The pass case is built from `/bin/ls` itself, so it holds on any host.

```bash
source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/check-elf.sh"
ls_bin=$(command -v ls)
ls_needed=$(readelf -d "$ls_bin" | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\]$/\1/p' | tr '\n' ' ')

CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=99.0 expect_exit 0 bash "$s" "$ls_bin"
expect_contains "NEEDED: " "$OUT"
expect_contains "GLIBC max: 2." "$OUT"

# Ceiling below what ls needs.
CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=2.0 expect_exit 1 bash "$s" "$ls_bin"
expect_contains "exceeds" "$ERR"

# Numeric, not lexical: 2.10 > 2.9.
CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=2.9 \
  STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_2.10) f" PATH="$FIXTURES/bin-elf:$PATH" \
  expect_exit 1 bash "$s" "$ls_bin"

# A library not on the allowlist.
CHECK_ELF_ALLOW="libc.so.6" CHECK_ELF_GLIBC_MAX=99.0 expect_exit 1 bash "$s" "$ls_bin"
expect_contains "not on the allowlist" "$ERR"

stub() { CHECK_ELF_ALLOW="$ls_needed" CHECK_ELF_GLIBC_MAX=99.0 PATH="$FIXTURES/bin-elf:$PATH" "$@"; }
STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_PRIVATE) f" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "GLIBC_PRIVATE" "$ERR"
STUB_OBJDUMP="0 DF *UND* 0 (GLIBC_ABI_DT_RELR) f" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "unrecognized" "$ERR"
STUB_OBJDUMP="" stub expect_exit 1 bash "$s" "$ls_bin"
expect_contains "no GLIBC_" "$ERR"
STUB_READELF_FAIL=1 stub expect_exit 1 bash "$s" "$ls_bin"
expect_exit 1 bash "$s" /nonexistent
expect_exit 2 bash "$s"

finish
```

- [ ] **Step 3: Run them and confirm they fail.**

Run: `bash scripts/release/tests/test-check-elf.sh`
Expected: FAIL lines, non-zero exit.

- [ ] **Step 4: Implement.**

```bash
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
```

- [ ] **Step 5: Run the tests.**

Run: `bash scripts/release/test.sh`
Expected: all files `0 failed`.

- [ ] **Step 6: Confirm a host build is rejected.** This is the reason the check runs in the container.

Run: `cargo build --release --locked && bash scripts/release/check-elf.sh target/release/tenuto; echo "exit=$?"`
Expected on this host (glibc 2.43): `check-elf: GLIBC_2.43 exceeds the 2.35 ceiling`, `exit=1`. Task 6 shows the container build passing.

- [ ] **Step 7: Commit.**

```bash
chmod +x scripts/release/*.sh scripts/release/fixtures/bin-elf/*
git add scripts/release
git commit -m "feat(release): ELF runtime-requirement check"
```

---

### Task 5: `smoke-test.sh`

**Files:**
- Create: `scripts/release/smoke-test.sh`
- Create: `scripts/release/fixtures/bin-tenuto/tenuto`
- Create: `scripts/release/tests/test-smoke-test.sh`

**Interfaces:**
- Consumes: Task 1's `--version` format.
- Produces: `smoke-test.sh <tenuto> <expected-version> <wav>` exits 0 when all five §9.1 checks pass, 1 on the first failure (naming the step), and 2 on a usage error.
- Stub contract (`fixtures/bin-tenuto/tenuto`): behaves like the real binary for the five checks. `STUB_TENUTO_MODE` of `wrong-version`, `writes-home`, `no-ended` or `no-state` breaks exactly one check each.

- [ ] **Step 1: Write the stub.**

```bash
#!/usr/bin/env bash
mode=${STUB_TENUTO_MODE:-ok}
case "${1:-}" in
  --version)
    if [ "$mode" = wrong-version ]; then echo "tenuto 9.9.9"; else echo "tenuto 1.2.3"; fi ;;
  --help)
    echo "A keyboard-first terminal audio player"; echo; echo "Usage: tenuto [COMMAND]" ;;
  feeds)
    [ "$mode" = writes-home ] && mkdir -p "$XDG_DATA_HOME/tenuto"
    echo "SLUG        EPISODES  REFRESHED (UTC)  TITLE" ;;
  play)
    [ "${TENUTO_AUDIO_OUTPUT:-}" = null ] || { echo "no audio device" >&2; exit 1; }
    echo "sine.wav [playing] 00:00:00"
    [ "$mode" = no-ended ] || echo "sine.wav [ended] 00:00:00"
    if [ "$mode" != no-state ]; then
      mkdir -p "$XDG_STATE_HOME/tenuto"; echo '{}' > "$XDG_STATE_HOME/tenuto/state.json"
    fi ;;
  *) exit 2 ;;
esac
```

Run `chmod +x scripts/release/fixtures/bin-tenuto/tenuto`. The tests execute it directly.

- [ ] **Step 2: Write the failing tests.**

```bash
source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/smoke-test.sh"
stub="$FIXTURES/bin-tenuto/tenuto"
wav="$RELEASE_DIR/../../tests/fixtures/sine.wav"

expect_exit 0 bash "$s" "$stub" 1.2.3 "$wav"
STUB_TENUTO_MODE=wrong-version expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "--version" "$ERR"
STUB_TENUTO_MODE=writes-home expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "not empty" "$ERR"
STUB_TENUTO_MODE=no-ended expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "[ended]" "$ERR"
STUB_TENUTO_MODE=no-state expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "state.json" "$ERR"
expect_exit 2 bash "$s" "$stub" 1.2.3

# A caller's TENUTO_AUDIO_OUTPUT must not leak into steps 1-4; play sets its own.
TENUTO_AUDIO_OUTPUT=cpal expect_exit 0 bash "$s" "$stub" 1.2.3 "$wav"

finish
```

- [ ] **Step 3: Run them and confirm they fail.**

Run: `bash scripts/release/tests/test-smoke-test.sh`
Expected: FAIL lines.

- [ ] **Step 4: Implement.**

```bash
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
```

- [ ] **Step 5: Run the tests.**

Run: `bash scripts/release/test.sh`
Expected: all files `0 failed`.

- [ ] **Step 6: Run it on the real binary.**

Run: `cargo build --release --locked && bash scripts/release/smoke-test.sh target/release/tenuto "$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="tenuto") | .version')" tests/fixtures/sine.wav`
Expected: `smoke-test: target/release/tenuto 0.1.2 passed all five checks`.

- [ ] **Step 7: Commit.**

```bash
chmod +x scripts/release/*.sh scripts/release/fixtures/bin-tenuto/tenuto
git add scripts/release
git commit -m "feat(release): smoke test for installed binaries, including a null-output play"
```

---

### Task 6: `.deb` metadata and `build-artifacts.sh`, verified in `ubuntu:22.04`

**Files:**
- Modify: `Cargo.toml` (append `[package.metadata.deb]`)
- Create: `scripts/release/build-artifacts.sh`

**Interfaces:**
- Consumes: `check-elf.sh` (Task 4).
- Produces: `build-artifacts.sh <sha> <outdir>`, run at the repository root with the pinned toolchain, `cargo-deb`, `jq`, `dpkg-dev` and `binutils` available. It writes `<outdir>/{tenuto-<ver>-x86_64-unknown-linux-gnu.tar.gz, tenuto_<ver>-1_amd64.deb, SHA256SUMS, build-info.txt}`. If `$GITHUB_OUTPUT` is set, it appends `version=<ver>`. It exits non-zero on any failed check.

- [ ] **Step 1: Add the metadata.** Append to `Cargo.toml` after `[package]` (before `[dependencies]`):

```toml
[package.metadata.deb]
section              = "sound"
priority             = "optional"
revision             = "1"
depends              = "$auto, ca-certificates"
extended-description = "Plays local files, HTTP media, live radio and podcast episodes from the terminal, and resumes each where you stopped."
assets = [
  ["target/release/tenuto", "usr/bin/", "755"],
  ["README.md", "usr/share/doc/tenuto/", "644"],
  ["CHANGELOG.md", "usr/share/doc/tenuto/", "644"],
  ["LICENSE", "usr/share/doc/tenuto/", "644"],
]
```

(`extended-description` is set because cargo-deb otherwise uses the whole README as the package description. `target/release/` is cargo-deb's special prefix and must stay literal.)

- [ ] **Step 2: Write the script.**

```bash
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
```

- [ ] **Step 3: Commit.** `build-artifacts.sh` checks `HEAD == sha`, so the build in Step 4 must run on a committed tree.

```bash
chmod +x scripts/release/build-artifacts.sh
git add Cargo.toml scripts/release/build-artifacts.sh
git commit -m "feat(release): .deb metadata and a one-build tarball-and-deb script"
```

- [ ] **Step 3b: Run it in the baseline container.** This is the same setup `package.yml` uses. A separate target dir keeps the host's `target/` clean, and the final `chown` returns the root-owned output to you.

```bash
sha=$(git rev-parse HEAD)
docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/src/target/ubuntu-22.04 \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" ubuntu:22.04 bash -euc '
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git ca-certificates curl build-essential pkg-config libasound2-dev dpkg-dev binutils jq >/dev/null
  git config --global --add safe.directory /src
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none >/dev/null
  . "$HOME/.cargo/env"
  rustup toolchain install 1.98.1 --profile minimal
  cargo install cargo-deb --locked --version 3.8.0
  status=0
  bash scripts/release/build-artifacts.sh '"$sha"' /src/target/dist || status=$?
  chown -R "$HOST_UID:$HOST_GID" /src/target/ubuntu-22.04 /src/target/dist
  exit $status
'
```

Expected: the output ends with `build-artifacts: wrote SHA256SUMS build-info.txt tenuto-0.1.2-x86_64-unknown-linux-gnu.tar.gz tenuto_0.1.2-1_amd64.deb`. If it fails, fix the cause, commit the fix, and rerun.

- [ ] **Step 4: Record what the real build produced.**

Run: `cat target/dist/build-info.txt`
Expected: `NEEDED:` equals the allowlist, or a subset of it; `GLIBC max:` ≤ 2.35; `deb_depends:` contains `libasound2 (>= …)`, `libc6 (>= …)` and `ca-certificates`. Copy the exact `NEEDED`, `GLIBC max` and `deb_depends` lines into the Task 10 notes. They are the values the docs quote. If `NEEDED` contains a library outside the allowlist, stop and ask the user before extending it.

- [ ] **Step 5: Confirm the crate still packages and stays under the cap.**

Run: `cargo publish --dry-run --locked && cargo package --locked --no-verify && stat -c%s target/package/tenuto-0.1.2.crate`
Expected: success, and a size below 10485760.

- [ ] **Step 6: Commit any fixes** made while getting Step 3b green, each as its own commit naming the failure.

---

### Task 7: `verify-deb-install.sh` and `verify-tarball-install.sh`, run on all five distros locally

**Files:**
- Create: `scripts/release/verify-deb-install.sh`, `scripts/release/verify-tarball-install.sh`

**Interfaces:**
- Consumes: the Task 6 outputs in a directory; `smoke-test.sh` next to the script; a WAV path.
- Produces: `verify-deb-install.sh <artifact-dir> <version> <wav>` and `verify-tarball-install.sh <artifact-dir> <version> <wav> <alsa-package>`, both run as root in a clean container. They exit 0 when every §5.1 check passes. They call `bash "$here/smoke-test.sh"`, because the executable bit does not survive artifacts.

- [ ] **Step 1: Write `verify-deb-install.sh`.**

```bash
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
```

- [ ] **Step 2: Write `verify-tarball-install.sh`.**

```bash
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
```

- [ ] **Step 3: Run the ten combinations locally.** The artifacts come from Task 6 (`target/dist`). Each container is fresh, and the repository is mounted read-only.

```bash
chmod +x scripts/release/verify-*.sh
v=0.1.2
for pair in debian:12=libasound2 debian:13=libasound2t64 ubuntu:22.04=libasound2 ubuntu:24.04=libasound2t64 ubuntu:26.04=libasound2t64; do
  img=${pair%%=*}; alsa=${pair#*=}
  docker run --rm -v "$PWD":/src:ro "$img" bash /src/scripts/release/verify-deb-install.sh /src/target/dist $v /src/tests/fixtures/sine.wav \
    && echo "OK deb $img" || echo "FAILED deb $img"
  docker run --rm -v "$PWD":/src:ro "$img" bash /src/scripts/release/verify-tarball-install.sh /src/target/dist $v /src/tests/fixtures/sine.wav "$alsa" \
    && echo "OK tarball $img" || echo "FAILED tarball $img"
done
```

`cd "$dir"` on a read-only mount is fine: `sha256sum -c` only reads, and `apt-get install ./file.deb` reads the file.

Expected: ten `OK` lines.

- [ ] **Step 4: If a `.deb` install fails to resolve ALSA on a t64 distro**, follow §6.2's fallback, not per-distro builds:
  1. `docker run --rm ubuntu:24.04 bash -c 'apt-get update -qq && apt-cache show libasound2t64 | grep -E "^(Provides|Version)"'`, and record the output.
  2. Set `depends` in `Cargo.toml` to the full `deb_depends` line from `build-info.txt`, with only the ALSA clause rewritten as `libasound2 (>= X) | libasound2t64 (>= X)` (same `X`), plus `, ca-certificates`. Remove `$auto`.
  3. Rerun Task 6 Step 3 and this step. Inspect `dpkg-deb -f target/dist/*.deb Depends`.
  4. If that still fails, stop and report to the user with the evidence. Per-distro builds are a design change.

- [ ] **Step 5: Commit.**

```bash
git add scripts/release/verify-deb-install.sh scripts/release/verify-tarball-install.sh Cargo.toml
git commit -m "feat(release): install checks for the .deb and the tarball in clean containers"
```

---

### Task 8: `package.yml` and the `ci.yml` wiring

**Files:**
- Create: `.github/workflows/package.yml`
- Modify: `.github/workflows/ci.yml` (rename job `package` → `crate`; add `linux-packages` and `release-scripts`)

**Interfaces:**
- Consumes: `build-artifacts.sh`, `verify-deb-install.sh`, `verify-tarball-install.sh`, `test.sh`, `find-rehearsal.sh`.
- Produces: the reusable workflow `./.github/workflows/package.yml`, with input `sha` (string, required) and output `version`. It uploads the artifacts `linux-x86_64-<sha>` and `test-inputs-<sha>`. Task 9 calls it.

- [ ] **Step 1: Write `package.yml`.**

```yaml
name: Linux packages

# Builds the x86_64 tarball and .deb once on Ubuntu 22.04, then installs
# those exact files on every tested distro (Linux packages spec §5.1).
# Called by ci.yml on every PR and by release.yml; never publishes.

on:
  workflow_call:
    inputs:
      sha:
        description: "Full SHA of the commit to package"
        required: true
        type: string
    outputs:
      version:
        description: "Package version read from Cargo.toml at sha"
        value: ${{ jobs.build.outputs.version }}

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always

jobs:
  build:
    name: Build (ubuntu:22.04)
    runs-on: ubuntu-latest
    container: ubuntu:22.04
    outputs:
      version: ${{ steps.build.outputs.version }}
    steps:
      - name: Install build prerequisites
        run: |
          apt-get update -qq
          DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
            git ca-certificates curl build-essential pkg-config libasound2-dev dpkg-dev binutils jq
      - uses: actions/checkout@v6
        with:
          ref: ${{ inputs.sha }}
          fetch-depth: 1
      - name: Mark the workspace safe for git
        run: git config --global --add safe.directory "$GITHUB_WORKSPACE"
      - name: Install pinned Rust toolchain
        run: |
          curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
          echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"
          "$HOME/.cargo/bin/rustup" toolchain install 1.98.1 --profile minimal
      - uses: Swatinem/rust-cache@v2
      - name: Install cargo-deb
        run: cargo install cargo-deb --locked --version 3.8.0
      - name: Build, inspect and package
        id: build
        run: bash scripts/release/build-artifacts.sh "${{ inputs.sha }}" dist
      - uses: actions/upload-artifact@v7
        with:
          name: linux-x86_64-${{ inputs.sha }}
          path: dist/
          if-no-files-found: error
          overwrite: true
      - uses: actions/upload-artifact@v7
        with:
          name: test-inputs-${{ inputs.sha }}
          path: |
            scripts/release/
            tests/fixtures/sine.wav
          if-no-files-found: error
          overwrite: true

  install-deb:
    name: Install .deb (${{ matrix.image }})
    needs: build
    runs-on: ubuntu-latest
    container: ${{ matrix.image }}
    strategy:
      fail-fast: false
      matrix:
        image: ["debian:12", "debian:13", "ubuntu:22.04", "ubuntu:24.04", "ubuntu:26.04"]
    steps:
      # No checkout: installing git would pull in ca-certificates as a
      # recommended package and defeat the dependency check.
      - uses: actions/download-artifact@v8
        with:
          name: linux-x86_64-${{ inputs.sha }}
          path: dist
      - uses: actions/download-artifact@v8
        with:
          name: test-inputs-${{ inputs.sha }}
          path: inputs
      - run: bash inputs/scripts/release/verify-deb-install.sh dist "${{ needs.build.outputs.version }}" inputs/tests/fixtures/sine.wav

  install-tarball:
    name: Install tarball (${{ matrix.image }})
    needs: build
    runs-on: ubuntu-latest
    container: ${{ matrix.image }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - { image: "debian:12", alsa: libasound2 }
          - { image: "debian:13", alsa: libasound2t64 }
          - { image: "ubuntu:22.04", alsa: libasound2 }
          - { image: "ubuntu:24.04", alsa: libasound2t64 }
          - { image: "ubuntu:26.04", alsa: libasound2t64 }
    steps:
      - uses: actions/download-artifact@v8
        with:
          name: linux-x86_64-${{ inputs.sha }}
          path: dist
      - uses: actions/download-artifact@v8
        with:
          name: test-inputs-${{ inputs.sha }}
          path: inputs
      - run: bash inputs/scripts/release/verify-tarball-install.sh dist "${{ needs.build.outputs.version }}" inputs/tests/fixtures/sine.wav "${{ matrix.alsa }}"
```

Upload-artifact keeps paths relative to the multi-path upload's common ancestor, which is the repository root. The test-inputs artifact therefore unpacks to `inputs/scripts/release/…` and `inputs/tests/fixtures/sine.wav`. If the first CI run shows a different layout, adjust the two `run:` paths, not the scripts.

- [ ] **Step 2: Edit `ci.yml`.** Rename the job key `package:` to `crate:` and its `name: Package` to `name: Crate package`. Change nothing else in it. Append:

```yaml
  linux-packages:
    name: Linux packages
    uses: ./.github/workflows/package.yml
    with:
      sha: ${{ github.sha }}

  release-scripts:
    name: Release scripts
    runs-on: ubuntu-latest
    permissions:
      contents: read
      actions: read
    steps:
      - uses: actions/checkout@v6
      - run: bash scripts/release/test.sh
      # Stubs check only the arguments the spec lists; this runs the real gh
      # against this repository so a wrong flag fails here, not at a tag.
      - name: Live rehearsal-lookup probe
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          set +e
          bash scripts/release/find-rehearsal.sh 0000000000000000000000000000000000000000
          code=$?
          set -e
          echo "find-rehearsal exited $code"
          test "$code" -eq 1
```

- [ ] **Step 3: Lint the workflows.**

Run: `docker run --rm -v "$PWD":/repo -w /repo rhysd/actionlint:latest -color`
Expected: no findings. If actionlint flags `shellcheck` issues in `run:` blocks, fix them. If it flags an unknown input on a newer action major, check that action's README and adjust.

- [ ] **Step 4: Commit.**

```bash
git add .github/workflows/package.yml .github/workflows/ci.yml
git commit -m "ci: build and install-test Linux packages on every PR; run the release-script tests"
```

---

### Task 9: `release.yml`

**Files:**
- Modify (full rewrite): `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `package.yml` (input `sha`, output `version`, artifact `linux-x86_64-<sha>`), `check-tag.sh`, `find-rehearsal.sh`, `changelog-section.sh`, `guard-release.sh`.
- Produces: rehearsal runs titled `Release rehearsal <sha>` that `find-rehearsal.sh` finds, and tag runs titled `Release <tag>` that publish.

- [ ] **Step 1: Write the file.**

```yaml
name: Release

# Rehearsal (workflow_dispatch on main, sha = main's HEAD) and release (a v*
# tag on a rehearsed main commit). Only the tag run's publish job writes.
# Sequence and recovery: docs/architecture.md §11 and the Linux packages
# spec §4.

on:
  workflow_dispatch:
    inputs:
      sha:
        description: "Full SHA of the main commit to rehearse (must be main's HEAD)"
        required: true
        type: string
  push:
    tags: ["v*"]

run-name: ${{ github.event_name == 'workflow_dispatch' && format('Release rehearsal {0}', inputs.sha) || format('Release {0}', github.ref_name) }}

permissions:
  contents: read

concurrency:
  group: release-${{ github.sha }}
  cancel-in-progress: false

env:
  CARGO_TERM_COLOR: always

jobs:
  validate:
    name: Validate
    runs-on: ubuntu-latest
    permissions:
      contents: read
      actions: read
    outputs:
      sha: ${{ steps.resolve.outputs.sha }}
      version: ${{ steps.resolve.outputs.version }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - id: resolve
        env:
          EVENT: ${{ github.event_name }}
          INPUT_SHA: ${{ inputs.sha }}
          GH_TOKEN: ${{ github.token }}
        run: |
          set -euo pipefail
          if [ "$EVENT" = workflow_dispatch ]; then
            [ "$GITHUB_REF" = refs/heads/main ] || { echo "rehearsals run on main, not $GITHUB_REF"; exit 1; }
            [[ "$INPUT_SHA" =~ ^[0-9a-f]{40}$ ]] || { echo "sha must be a full 40-character SHA"; exit 1; }
            [ "$INPUT_SHA" = "$GITHUB_SHA" ] || { echo "sha $INPUT_SHA is not main's HEAD at dispatch ($GITHUB_SHA)"; exit 1; }
            sha=$INPUT_SHA
          else
            sha=$(git rev-parse "$GITHUB_REF^{commit}")
          fi
          git merge-base --is-ancestor "$sha" origin/main || { echo "$sha is not on main"; exit 1; }
          version=$(git show "$sha:Cargo.toml" | python3 -c 'import sys, tomllib; print(tomllib.loads(sys.stdin.read())["package"]["version"])')
          if [ "$EVENT" = push ]; then
            bash scripts/release/check-tag.sh "$GITHUB_REF_NAME" "$version"
            bash scripts/release/find-rehearsal.sh "$sha"
          fi
          git show "$sha:CHANGELOG.md" > "$RUNNER_TEMP/CHANGELOG.md"
          bash scripts/release/changelog-section.sh "$version" "$RUNNER_TEMP/CHANGELOG.md"
          echo "sha=$sha" >> "$GITHUB_OUTPUT"
          echo "version=$version" >> "$GITHUB_OUTPUT"

  suite:
    name: Test and publish dry run
    needs: validate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          ref: ${{ needs.validate.outputs.sha }}
      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --locked --no-fail-fast
      - run: cargo publish --dry-run --locked

  package:
    name: Linux packages
    needs: validate
    uses: ./.github/workflows/package.yml
    with:
      sha: ${{ needs.validate.outputs.sha }}

  publish:
    name: Publish GitHub Release
    needs: [validate, suite, package]
    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')
    runs-on: ubuntu-latest
    permissions:
      contents: write
    env:
      GH_TOKEN: ${{ github.token }}
      GH_REPO: ${{ github.repository }}
      TAG: ${{ github.ref_name }}
      VERSION: ${{ needs.validate.outputs.version }}
    steps:
      - uses: actions/checkout@v6
        with:
          ref: ${{ needs.validate.outputs.sha }}
      - uses: actions/download-artifact@v8
        with:
          name: linux-x86_64-${{ needs.validate.outputs.sha }}
          path: dist
      - name: Verify checksums
        run: cd dist && sha256sum -c SHA256SUMS
      - name: Refuse to overwrite an existing release
        run: bash scripts/release/guard-release.sh "$TAG"
      - name: Release notes
        run: bash scripts/release/changelog-section.sh "$VERSION" > "$RUNNER_TEMP/notes.md"
      - name: Create draft and upload
        run: |
          gh release create "$TAG" --verify-tag --draft \
            --title "tenuto $VERSION" --notes-file "$RUNNER_TEMP/notes.md" \
            "dist/tenuto-$VERSION-x86_64-unknown-linux-gnu.tar.gz" \
            "dist/tenuto_$VERSION-1_amd64.deb" \
            dist/SHA256SUMS dist/build-info.txt
      - name: Publish
        run: gh release edit "$TAG" --draft=false
```

Why validate reads files with `git show "$sha:…"`: on dispatch, the checkout is `GITHUB_SHA`, which equals `sha`. On a tag push, the checkout is the tag's commit. Reading through `$sha` explicitly keeps the check tied to the resolved commit either way.

- [ ] **Step 2: Lint.**

Run: `docker run --rm -v "$PWD":/repo -w /repo rhysd/actionlint:latest -color`
Expected: no findings.

- [ ] **Step 3: Dry-check the validate logic for a tag locally** against the real changelog. This catches a wrong section format before any tag exists:

Run: `bash scripts/release/check-tag.sh v0.1.2 0.1.2 && bash scripts/release/changelog-section.sh 0.1.2 >/dev/null && echo ok`
Expected: `check-tag: v0.1.2 matches Cargo.toml 0.1.2` and `ok`.

- [ ] **Step 4: Commit.**

```bash
git add .github/workflows/release.yml
git commit -m "ci: rehearsal and tag-driven GitHub Release with the Linux packages"
```

---

### Task 10: Documentation

**Files:**
- Modify: `docs/architecture.md` (§11: the deployment sentence, the build-requirements table, the release paragraph)
- Modify: `README.md` (Install)
- Modify: `CHANGELOG.md` (Unreleased → Added)

**Interfaces:**
- Consumes: the `NEEDED`, `GLIBC max` and `deb_depends` values recorded in Task 6 Step 4. Written below as `<NEEDED>`, `<GLIBC>` and `<DEPENDS>`. Substitute the recorded values; never leave the angle-bracket names in the docs.

- [ ] **Step 1: `docs/architecture.md` §11, first sentence.** Replace:

`There is one deployable: a statically linked \`tenuto\` binary per platform. No installer, no service, no configuration file is required.`

with:

```markdown
There is one deployable: a `tenuto` binary per platform. On Linux it links glibc and ALSA dynamically: the x86_64 release binary needs glibc <GLIBC> or newer and the shared libraries <NEEDED>, as reported by `scripts/release/check-elf.sh`. HTTPS also needs the system CA bundle (`ca-certificates`). No installer, no service, no configuration file is required.
```

- [ ] **Step 2: Gates row.** In the build-requirements table, append to the `Gates` cell: `, and the Linux package build on Ubuntu 22.04 with installation and smoke tests of the .deb and the tarball on Debian 12 and 13 and Ubuntu 22.04, 24.04 and 26.04 (\`.github/workflows/package.yml\`)`.

- [ ] **Step 3: Release paragraph.** Replace the paragraph that starts `The crate ships to crates.io as \`tenuto\`` and ends `stay out of it.` with:

```markdown
The crate ships to crates.io as `tenuto`, the same name as the published
binary and the library target. The package excludes `/tests`, `/docs` and
`/scripts`, so the 17 MB of audio fixtures stay out of it.

A release has five steps:

1. Merge the release PR (version bump and changelog entry) into `main`.
2. Dispatch `.github/workflows/release.yml` on `main` with `sha` set to that
   merge commit, which must be `main`'s HEAD. This rehearsal runs the suite,
   a publish dry run, the Linux packages and their install matrix, and checks
   the changelog entry. It publishes nothing.
3. `cargo publish --locked` from a clean checkout of that commit, with a
   maintainer's own crates.io token.
4. Push `vX.Y.Z` pointing at that commit.
5. The tag run checks the tag against `Cargo.toml` and requires a successful
   rehearsal of the same commit. It then rebuilds and reinstalls the packages
   on every tested distro, and publishes a GitHub Release with the tarball,
   the `.deb`, `SHA256SUMS` and `build-info.txt` that run tested.

Tags are immutable. A transient failure is rerun on the same tag. A source or
packaging fix is a new patch release, which also costs a crates.io version.
If `publish` fails part-way, it may leave a draft release: inspect it, delete
it, and rerun the job. A published release is never overwritten. Archive
metadata is normalized; reproducible builds are not guaranteed.
```

- [ ] **Step 4: `README.md` Install.** Insert after the `cargo install` paragraph (after the "the runtime `libasound.so.2` alone is not enough." line):

````markdown
### Prebuilt packages (x86_64 Linux)

Each [GitHub Release](https://github.com/alvytsk/tenuto/releases) carries
a `.deb`, a tarball, `SHA256SUMS` and `build-info.txt`. Verify the download
first:

```sh
sha256sum -c SHA256SUMS --ignore-missing
```

**Debian and Ubuntu.** Tested on Debian 12 and 13; Ubuntu 22.04, 24.04 and 26.04.

```sh
sudo apt install ./tenuto_<version>-1_amd64.deb
```

**Tarball.** Built on Ubuntu 22.04 and tested on the same five releases. It
needs glibc <GLIBC> or newer, the ALSA runtime library (`libasound2`, or
`libasound2t64` on Debian 13 and Ubuntu 24.04 and later) and
`ca-certificates`. The binary links exactly <NEEDED>.

```sh
tar -xzf tenuto-<version>-x86_64-unknown-linux-gnu.tar.gz
./tenuto-<version>-x86_64-unknown-linux-gnu/tenuto --version
```
````

`<version>` stays literal: it is a user-facing placeholder. `<GLIBC>` and `<NEEDED>` are substituted from Task 6.

- [ ] **Step 5: `CHANGELOG.md`.** Under `## [Unreleased]` → `### Added`, below the `--version` entry:

```markdown
- Prebuilt x86_64 Linux packages on each GitHub Release from the next
  release on: a `.deb`, tested on Debian 12 and 13 and Ubuntu 22.04, 24.04
  and 26.04, and a tarball, with `SHA256SUMS` and `build-info.txt`.
```

Do not touch the link references at the bottom. `[0.1.2]` stays commit-based. The first tagged release's PR rewrites `[Unreleased]` and adds its own compare link (spec §10).

- [ ] **Step 6: Check for leftovers.**

Run: `grep -nE '<(GLIBC|NEEDED|DEPENDS)>|statically linked|No tags are pushed' docs/architecture.md README.md CHANGELOG.md`
Expected: no output.

- [ ] **Step 7: Commit.**

```bash
git add docs/architecture.md README.md CHANGELOG.md
git commit -m "docs: Linux packages, the tagged release sequence, and the real runtime requirements"
```

---

### Task 11: Full local gate, PR and first CI run

**Files:** none new (fixes only, if CI finds something).

- [ ] **Step 1: The full local gate.**

Run:

```bash
cargo fmt --check \
&& cargo clippy --locked --all-targets --all-features -- -D warnings \
&& cargo test --locked --no-fail-fast \
&& RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps \
&& cargo publish --dry-run --locked \
&& bash scripts/release/test.sh
```

Expected: all pass. Report any failure verbatim; don't paper over it.

- [ ] **Step 2: Ask the user before pushing.** Pushing the branch and opening a PR are outward-facing. On their go:

```bash
git push -u origin feat/linux-packages
gh pr create --title "feat: Linux packages (.deb and tarball) and tag-driven GitHub Releases" --body-file <(printf '%s\n' \
  "Implements docs/superpowers/specs/2026-09-22-tenuto-linux-packages-design.md." "" \
  "- package.yml: one build on ubuntu:22.04, ELF and Depends checks, .deb + tarball + SHA256SUMS + build-info.txt" \
  "- install matrix: the built .deb and tarball on Debian 12/13 and Ubuntu 22.04/24.04/26.04, with a null-output play smoke test" \
  "- release.yml: rehearsal on main's HEAD; v* tag requires a matching successful rehearsal, then publishes a GitHub Release" \
  "- scripts/release: tested decision logic with an argument-checking gh stub, plus a live gh probe in CI" \
  "- tenuto --version")
```

- [ ] **Step 3: Watch the PR checks.** Run `gh pr checks --watch`. Expected: `Linux packages / Build`, the ten install jobs, `Release scripts` (including the live probe exiting 1) and every existing job green. The macOS test leg is still non-blocking.

- [ ] **Step 4: Fix CI-only failures.** Likely candidates:
  - Artifact layout differs from `inputs/scripts/release/…`: adjust the `run:` paths in `package.yml`.
  - `ubuntu:26.04` image missing on Docker Hub: stop and report. The tested-distro list is a spec decision.
  - rust-cache misbehaves in the container: remove that step, keep everything else, and note the slower build in the PR.

  Commit each fix separately with a message naming the failure.

- [ ] **Step 5: Hand back.** Report the PR URL, the CI results, and the recorded `NEEDED`, `GLIBC max` and `Depends` lines. State plainly that the tag and `publish` path runs for the first time at the next real release (spec §9.4). Merging and the rehearsal dispatch are the user's to do. After merge, update the memory note `tenuto-release-pipeline.md`: tags now exist, release sequence per architecture §11, and the first tag run is the first live test of `publish`.
