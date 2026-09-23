# Tenuto: Linux packages and GitHub Releases

Status: approved in conversation on 2026-09-22; ready for an implementation plan. The behavior below is the proposed contract, not an assertion that it already exists.

Branch: `feat/linux-packages`.

## 1. Product decision

Tenuto installs today only through `cargo install tenuto`, which needs a Rust toolchain and the ALSA headers. This work adds prebuilt Linux packages, attached to a GitHub Release per version:

- `tenuto-<ver>-x86_64-unknown-linux-gnu.tar.gz`: the binary and its docs, for any glibc distro that meets the inspected baseline.
- `tenuto_<ver>-1_amd64.deb`: for Debian and Ubuntu.
- `SHA256SUMS` and `build-info.txt`: checksums, source SHA and the inspected runtime requirements.

Every artifact is built once per run on Ubuntu 22.04 and installed, as built, on each tested distro before it can be published.

Out of scope for this iteration: aarch64 (the next step, on native ARM runners), `.rpm`, distro repositories (apt repo, AUR, Homebrew), AppImage/Flatpak/Snap, crates.io automation, and reproducible builds.

> **Superseded, crates.io automation.** A later change moved the publish into
> the tag run as a `publish-crate` job, using Trusted Publishing and a
> reviewed `crates-io` environment. §4 below records the sequence as designed
> here; `docs/architecture.md` §11 is the current one.

## 2. Existing foundations

- `.github/workflows/ci.yml` runs fmt, clippy, the test suite (Linux blocking, macOS non-blocking), doc, and a `package` job that runs `cargo publish --dry-run --locked` and checks the `.crate` stays under 10 MB.
- `.github/workflows/release.yml` is a `workflow_dispatch` rehearsal only: the suite plus a publish dry run. Publishing to crates.io is `cargo publish --locked` by hand from a clean checkout of `main`.
- No git tags and no GitHub Releases exist. `CHANGELOG.md` compare links use commit SHAs: `[0.1.2]` is `29da62c...855fcb5` and `[Unreleased]` is `855fcb5...HEAD`.
- `docs/architecture.md` §11 says "a statically linked `tenuto` binary per platform". That is wrong: CPAL links `libasound.so.2` dynamically, and glibc is dynamic.
- There is no `[profile.release]`. Release builds use Cargo's defaults (`debug = false`, no strip).
- `src/cli.rs:8` declares `#[command(name = "tenuto", about = ...)]` without `version`, so `tenuto --version` exits 2 with a usage error.
- `tenuto feeds` goes through `platform_subscription_stores` (`src/commands.rs:114`), whose constructors touch no files, and never opens `state.json`. Run with `HOME` and all four `XDG_*` directories pointed at an empty temporary directory, it prints the header row `SLUG  EPISODES  REFRESHED (UTC)  TITLE`, exits 0 and creates nothing (checked 2026-09-22).
- HTTP is reqwest with rustls, so there is no OpenSSL dependency.

## 3. Supported and tested platforms

Build baseline: an `ubuntu:22.04` container.

Tested install targets, the only ones the docs name: **Debian 12 and 13; Ubuntu 22.04, 24.04 and 26.04**, all on x86_64. The docs do not say "12+" or "22.04+".

The documented runtime requirements are whatever `check-elf.sh` (§6) reports for the release binary: the highest required `GLIBC_` version and the full `NEEDED` list. They are not assumed from the build host. One requirement is not a shared library and is declared by hand: the system CA bundle (`ca-certificates`), which HTTPS needs at run time (§6.2). The expectation is glibc ≤ 2.35 plus `libasound.so.2`, `libc.so.6`, `libm.so.6` and `libgcc_s.so.1`. The first real build fixes the list, and the docs quote it.

## 4. Release sequence and policy

1. Merge the release PR (version bump and changelog entry) into `main`.
2. Dispatch `release.yml` on `main` with `sha` set to that merge commit. This is the rehearsal: the full suite, a crates.io dry run, the packages and the full install matrix, on that exact commit. Nothing is published.
3. Publish to crates.io by hand: `cargo publish --locked` from a clean checkout of that commit.
4. Push `vX.Y.Z` pointing at that same commit.
5. The tag run validates, rebuilds, reruns the full matrix and, only if every required job passes, publishes the GitHub Release with the artifacts that run built and tested.

Tags are immutable markers of what shipped:

- **Transient failure** (runner, network, registry): rerun the failed jobs of the tag run. The tag stays unchanged.
- **Source or packaging fix**: make a new patch release. There is no package-revision scheme, and tags are never moved or reused. This is a deliberate maintenance cost: a packaging-only correction also costs a crates.io version. In return, one version always means one source commit and one set of artifacts. Revisit only if packaging-only releases become frequent.
- **Publication failure part-way through**: failed prerequisites stop publication from starting, but creating a release and uploading its assets takes several API calls, so a failure in `publish` itself can leave an incomplete draft release. Recovery: inspect the draft, delete it, rerun `publish` on the unchanged tag. A published release is never overwritten (§7.4).

## 5. Workflow layout

Three workflow files, plus the scripts in §6–§8.

### 5.1 `package.yml` (reusable)

- Trigger: `workflow_call` only. Input: `sha` (string, required). Output: `version`.
- `permissions: contents: read`.
- Relative `uses: ./.github/workflows/package.yml` resolves at the caller's commit, so the caller, the called workflow and the source are one commit.

Jobs:

1. **`build`**, `runs-on: ubuntu-latest` in `container: ubuntu:22.04`, `timeout-minutes: 60`.
   - Installs `git ca-certificates curl build-essential pkg-config libasound2-dev dpkg-dev binutils jq`, marks the workspace `safe.directory`, and checks out `inputs.sha` with `fetch-depth: 1`.
   - Installs Rust with rustup from `rust-toolchain.toml` (1.98.1), then `cargo install cargo-deb --locked --version <pinned>`. `Swatinem/rust-cache@v2` caches the build.
   - Reads the version from `cargo metadata` and exposes it as the `version` output.
   - Builds once: `CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked`. The project's release profile is unchanged, so `cargo install` users keep symbols.
   - `scripts/release/check-elf.sh target/release/tenuto`, whose report is saved for `build-info.txt`.
   - `cargo deb --no-build --no-strip`, which packages the same bytes.
   - `dpkg-deb -f <deb> Depends` is printed and checked: it must name the ALSA library with a version constraint, `libc6 (>= …)` and `ca-certificates`.
   - The tarball (§8.2).
   - Byte identity: the SHA-256 of `target/release/tenuto`, of `./usr/bin/tenuto` extracted from the `.deb` (`dpkg-deb -x`), and of the binary extracted from the tarball must all be equal.
   - Writes `SHA256SUMS` (tarball and `.deb`) and `build-info.txt` (source SHA, version, rustc version, cargo-deb version, the `.deb` `Depends` line, the `check-elf.sh` report, the binary's SHA-256).
   - Uploads one artifact, `linux-x86_64-<sha>`, with the four files. It also uploads a second, never-published artifact, `test-inputs-<sha>`, with `scripts/release/smoke-test.sh` and `tests/fixtures/sine.wav` from the same checkout.
2. **`install-deb`**, which `needs: build`, `timeout-minutes: 15`, in a matrix of `container:` `debian:12`, `debian:13`, `ubuntu:22.04`, `ubuntu:24.04`, `ubuntu:26.04`, `fail-fast: false`.
   - Downloads the artifact and runs `sha256sum -c SHA256SUMS`.
   - Downloads `test-inputs-<sha>`. There is no checkout: installing `git` in the container would pull in `ca-certificates` as a recommended package and invalidate the next check.
   - Asserts `ca-certificates` is **not** installed yet, then runs `apt-get update && apt-get install -y ./tenuto_<ver>-1_amd64.deb`, which resolves ALSA and `ca-certificates` from the distro's own archive.
   - `dpkg -s tenuto` and `dpkg -s ca-certificates` must both report `Status: install ok installed`, and `/etc/ssl/certs/ca-certificates.crt` must exist and be non-empty.
   - `smoke-test.sh /usr/bin/tenuto <ver> sine.wav` from the downloaded test inputs.
   - `apt-get remove -y tenuto`, then `/usr/bin/tenuto` must not exist.
3. **`install-tarball`**, which `needs: build`, `timeout-minutes: 15`, in the same five-container matrix.
   - Installs only the runtime prerequisites the README lists for the tarball: the ALSA runtime library, named per matrix entry (`libasound2` on Debian 12 and Ubuntu 22.04, `libasound2t64` on Debian 13, Ubuntu 24.04 and 26.04), and `ca-certificates`. No toolchain, no `-dev` packages.
   - Asserts `/etc/ssl/certs/ca-certificates.crt` exists and is non-empty.
   - Verifies checksums, downloads `test-inputs-<sha>`, extracts the tarball and runs `smoke-test.sh` on the extracted `tenuto`.

No test container installs Rust or rebuilds anything. The release binary and packages under test come only from the release artifact. The test-inputs artifact supplies only the test script and one audio fixture.

### 5.2 `ci.yml`

- The existing crates.io `package` job is renamed `crate` (display name "Crate package"), unchanged otherwise.
- New job `linux-packages`: `uses: ./.github/workflows/package.yml` with `sha: ${{ github.sha }}`, on every PR and push to `main`.
- New job `release-scripts`: runs `scripts/release/test.sh` (§9.2) on `ubuntu-latest`, `timeout-minutes: 10`.
- Top-level `concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: ${{ github.event_name == 'pull_request' }} }`, so a superseded PR push cancels the older run while a push to `main` never does.

### 5.3 `release.yml`

Replaces the current rehearsal.

```yaml
on:
  workflow_dispatch:
    inputs:
      sha: { description: "Full SHA of the main commit to rehearse (must be main's HEAD)", required: true }
  push:
    tags: ["v*"]
run-name: ${{ github.event_name == 'workflow_dispatch' && format('Release rehearsal {0}', inputs.sha) || format('Release {0}', github.ref_name) }}
permissions: { contents: read }
concurrency: { group: release-${{ github.sha }}, cancel-in-progress: false }
```

Jobs:

1. **`validate`** always runs, on both events. `timeout-minutes: 10`. Checkout has `fetch-depth: 0`. Outputs: `sha`, `version`.
   - Dispatch: `github.ref == refs/heads/main`; `inputs.sha` matches `^[0-9a-f]{40}$`; `inputs.sha == github.sha` (the SHA captured at dispatch, not a freshly fetched `main`, so a merge while the run queues cannot invalidate it); `git merge-base --is-ancestor $sha origin/main`.
   - Tag push: `scripts/release/check-tag.sh "$GITHUB_REF_NAME" "$version"` (the tag must be exactly `v` + the `Cargo.toml` version at the tagged commit); the commit is an ancestor of `origin/main`; `scripts/release/find-rehearsal.sh "$sha"` exits 0 (§7.2). This job gets `actions: read` for the lookup.
   - Both events: `scripts/release/changelog-section.sh "$version"` must succeed on the real `CHANGELOG.md` at that commit. A missing, duplicated or empty section therefore fails the rehearsal, before the crates.io publish and the tag, not in `publish` after both.
2. **`suite`** needs `validate`: `timeout-minutes: 60`. `cargo test --locked --no-fail-fast` and `cargo publish --dry-run --locked` at `validate.outputs.sha`, on `ubuntu-latest` with ALSA headers, as `ci.yml` does it.
3. **`package`** needs `validate`: `uses: ./.github/workflows/package.yml` with `sha: ${{ needs.validate.outputs.sha }}`.
4. **`publish`**: `needs: [validate, suite, package]`, `if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')`, `timeout-minutes: 15`, `permissions: contents: write`. The only job with write access (§7.4).

No job is dispatch-only, so on a tag run nothing that `publish` needs is skipped. On a dispatch run, `publish` is skipped by its own `if` and the run is green when everything else passes.

A rehearsal must run on `main`'s HEAD. A new rehearsal of an older commit is refused. A successful rehearsal that already exists still qualifies after `main` moves on.

## 6. Binary and `.deb`

### 6.1 `scripts/release/check-elf.sh <binary>`

`set -euo pipefail`. Runs `readelf -d` and `objdump -T` and fails if:

- either tool exits non-zero, or produces no `NEEDED` entries or no `GLIBC_` version tokens;
- any version token is `GLIBC_PRIVATE`, or does not match `^GLIBC_[0-9]+(\.[0-9]+)+$` (for example, a form nobody has reviewed);
- the highest version, compared numerically (`sort -V`, so `GLIBC_2.2.5` < `GLIBC_2.17` < `GLIBC_2.35`), exceeds `2.35`;
- any `NEEDED` entry is missing from the allowlist in the script. The allowlist is explicit library names, fixed from the first real build. Adding a name is a reviewed change.

It prints the `NEEDED` list and the highest `GLIBC_` version. To check locally, run it inside the `ubuntu:22.04` container on a binary built there. A host-built binary may legitimately exceed the baseline.

### 6.2 `[package.metadata.deb]`

```toml
[package.metadata.deb]
section  = "sound"
priority = "optional"
revision = "1"
depends  = "$auto, ca-certificates"
assets = [
  ["target/release/tenuto", "usr/bin/", "755"],
  ["README.md",    "usr/share/doc/tenuto/", "644"],
  ["CHANGELOG.md", "usr/share/doc/tenuto/", "644"],
  ["LICENSE",      "usr/share/doc/tenuto/", "644"],
]
```

The maintainer, description, license and homepage come from the existing manifest fields. `$auto` runs `dpkg-shlibdeps` on 22.04, which is expected to produce `libasound2 (>= 1.0.16), libc6 (>= 2.35)` and more. The build log records the actual line.

`ca-certificates` is declared by hand because `dpkg-shlibdeps` sees only shared libraries. The locked reqwest uses `rustls-platform-verifier` (`Cargo.lock`), which on Linux reads the system CA bundle at run time. Without it, every HTTPS feed, episode and stream fails certificate verification while the CLI still starts. The official `debian` and `ubuntu` images ship without `ca-certificates`, so `install-deb` proves the dependency pulls it in.

**ALSA fallback**, only if the Ubuntu 24.04 or 26.04 or Debian 13 install fails to resolve ALSA. First check the `Provides:` of `libasound2t64` on the failing distro. If an alternative is needed, **replace** the generated ALSA clause, not append to it: set `depends` explicitly to the full `$auto` output with the ALSA clause rewritten as `libasound2 (>= X) | libasound2t64 (>= X)`, keeping every other generated clause and version constraint. Then inspect the final control data with `dpkg-deb -f`. Per-distro builds are considered only if that fails too, and the matrix's evidence is recorded in the PR.

## 7. Publication logic

The decision logic lives in scripts under `scripts/release/`, not inline YAML, so §9.2 can test it without publishing.

### 7.1 `check-tag.sh <tag> <version>`

Exit 0 only if `tag == "v$version"`. Rejects `v0.1.3-rc1` against `0.1.3`, `0.1.3` without the `v`, and a tag for a different version.

### 7.2 `find-rehearsal.sh <sha>`

Calls:

```sh
workflow_id=$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/release.yml" --jq .id)
gh run list --workflow release.yml --event workflow_dispatch --branch main \
  --commit "$sha" --status success --limit 200 \
  --json databaseId,headSha,headBranch,event,conclusion,workflowDatabaseId,displayTitle,url
```

(`gh workflow view` has no `--json` flag. It fails with `unknown flag: --json`, which is why the workflow ID comes from the REST API.)

It re-filters the result in `jq` on every field, so a server-side filter that stopped working cannot let a wrong run through: `workflowDatabaseId == $workflow_id`, `event == "workflow_dispatch"`, `headBranch == "main"`, `headSha == $sha`, `conclusion == "success"`. The title is never matched on. `url` is requested because exit 0 prints the matched run's URL.

Exit codes are a contract:

- `0`: at least one qualifying run. Prints its URL.
- `1`: the lookup succeeded, and no qualifying run exists. Prints which rehearsal is missing.
- `2`: the lookup itself failed (a `gh` error, non-JSON output, empty `workflow_id`).

`validate` treats anything but `0` as failure. The distinction lets the probe in §9.2 tell "none found" from "the command is broken".

### 7.3 `changelog-section.sh <version>`

Prints the body of `## [<version>] - YYYY-MM-DD` from `CHANGELOG.md`, up to the next `## ` heading, without the heading. Fails if the heading is missing, appears twice, or has an empty body. The output is the release notes.

### 7.4 The `publish` job

1. Download artifact `linux-x86_64-<sha>` from **this run** and run `sha256sum -c SHA256SUMS`.
2. `scripts/release/guard-release.sh "$tag"`, using `gh release view`:
   - If no release exists, proceed.
   - If a draft exists, fail: "an incomplete draft for `<tag>` exists; inspect and delete it, then rerun".
   - If a published release exists, fail: never overwrite.
3. `changelog-section.sh "$version" > notes.md`.
4. `gh release create "$tag" --verify-tag --draft --title "tenuto $version" --notes-file notes.md <tarball> <deb> SHA256SUMS build-info.txt`
5. `gh release edit "$tag" --draft=false`.

Step 4 creates the draft and uploads the four files. Step 5 makes it public only after all four uploads succeed.

## 8. Artifacts

### 8.1 Naming

- `tenuto-<ver>-x86_64-unknown-linux-gnu.tar.gz`
- `tenuto_<ver>-1_amd64.deb` (cargo-deb's naming)
- `SHA256SUMS`: `sha256sum` output for the tarball and the `.deb`.
- `build-info.txt`: see §5.1.

### 8.2 Tarball

```
tenuto-<ver>-x86_64-unknown-linux-gnu/
  tenuto          (0755)
  README.md       (0644)
  CHANGELOG.md    (0644)
  LICENSE         (0644)
```

Built with `tar --format=gnu --sort=name --owner=0 --group=0 --numeric-owner --mode='u=rwX,go=rX' --mtime=@<commit time>`, with explicit `chmod` on the staged files, piped through `gzip -n`. Commit time is `git show -s --format=%ct <sha>`. The archive metadata is normalized. Reproducible builds are not guaranteed: the compiled binary can differ when container packages or other build inputs change.

## 9. Verification

### 9.1 `scripts/release/smoke-test.sh <tenuto> <expected-version> <wav-fixture>`

`set -euo pipefail`. Creates a temporary `$root`, with a `trap` that removes it on exit. Inside it are `$root/home` and `$root/play-home` (isolated directories) and `$root/out` (captured output, kept outside the directories whose contents are checked). Each command runs with the ambient `PATH`, `HOME` set to its isolated directory, and `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` all under it.

Under `$root/home`:

1. `tenuto --version` exits 0 and prints exactly `tenuto <expected-version>`.
2. `tenuto --help` exits 0, and its output contains `Usage: tenuto`.
3. `tenuto feeds` exits 0, prints exactly one line, and that line starts with `SLUG`. This exercises subscription loading, not playback-state loading: the command never opens `state.json`.
4. `$root/home` is empty afterwards.

Under `$root/play-home`, with `TENUTO_AUDIO_OUTPUT=null`, stdin from `/dev/null`, and a `timeout 30`:

5. `tenuto play <wav-fixture>` exits 0, its output contains `[ended]`, and `$XDG_STATE_HOME/tenuto/state.json` exists afterwards.

The null output is the paced deviceless output the process tests already use (`src/playback/output/null_output.rs`). It is selected at run time by the environment variable in `EngineHandle::spawn_for_environment` (`src/playback/engine.rs:242`) and compiled into the release binary. On `tests/fixtures/sine.wav` (88 KB), this finished in 0.6 s with exit 0, `[ended]` and a written `state.json`, both with and without a TTY (checked 2026-09-22). The fixture is WAV because it needs the fewest decoder paths. MP3, FLAC and M4A stay covered by the suite.

Steps 1–4 establish installation and startup compatibility: the loader resolves every `NEEDED` library at a compatible version, and the binary runs its CLI. Step 5 adds that the shipped binary decodes, runs the transport to the end and writes state. The one piece it doesn't reach is the CPAL/ALSA device path, since containers have no sound device. HTTPS is not exercised either: the install jobs only check that the CA bundle is present (§5.1), and there is no network dependency.

`--version` is a version check, not an identity check: different commits can report the same version. Identity comes from `build-info.txt` (source SHA) and `SHA256SUMS`, and `publish` uploads the exact files the tag run's matrix installed.

### 9.2 `scripts/release/test.sh`

Plain shell tests with fixtures under `scripts/release/fixtures/`. `gh` is stubbed by a script put first on `PATH` that returns fixture JSON or exit codes. The stub validates its arguments: it accepts only the exact subcommands and flags each script is specified to use (§7.2, §7.4), and it exits non-zero with `unknown flag` on anything else, as the real `gh` does. A stub that accepted any arguments would have hidden the `gh workflow view --json` mistake. The tests cover:

- `check-tag.sh`: match; missing `v`; pre-release suffix; wrong version.
- `find-rehearsal.sh`: a qualifying run (exit 0); runs that fail on each field in turn (other workflow ID, `push` event, other branch, other SHA, `failure` conclusion) and an empty list (exit 1); `gh` failing and non-JSON output (exit 2, never 0).
- `changelog-section.sh`: an existing version; a missing version; an empty body; `[Unreleased]` not returned for a version query.
- `guard-release.sh`: not found; draft; published; `gh` failing for another reason (must fail).
- `check-elf.sh`: run against `/bin/ls` with the allowlist and ceiling overridden through `CHECK_ELF_ALLOW` and `CHECK_ELF_GLIBC_MAX` (test-only overrides; the workflow never sets them), one run that must pass and one with a ceiling below `/bin/ls`'s that must fail, plus a stubbed `objdump` producing an unparseable version token, and empty output.

`ci.yml` runs them in the `release-scripts` job. Stubs can only check the arguments the spec lists, not whether the real `gh` accepts them. So the same job, with `permissions: actions: read` and `GH_TOKEN: ${{ github.token }}`, also runs a **live probe**: `find-rehearsal.sh 0000000000000000000000000000000000000000` against the real repository, with the runner's real `gh`, must exit `1` (none found), not `2`. That exercises the real `gh api` and `gh run list` flags on every PR, long before a tag depends on them.

### 9.3 Rust

`#[command(version)]` in `src/cli.rs`, and a test in `tests/cli.rs` asserting that `tenuto --version` prints `tenuto <CARGO_PKG_VERSION>` and exits 0.

### 9.4 What is and isn't tested before the first release

Before merge: all scripts locally (`check-elf.sh` inside `ubuntu:22.04`), `test.sh`, and the PR's `linux-packages` job running the full build and the 10-job install matrix on real runners.

The first real release is the first end-to-end test of the GitHub write path: `gh release create`, the asset uploads and the draft-to-published edit. Its decision logic (tag check, rehearsal lookup, changelog extraction, existing-release guard) is covered by §9.2 beforehand. The spec does not claim more.

## 10. Documentation changes

- **`docs/architecture.md` §11**: replace "statically linked" with the inspected runtime requirements (§3). Rewrite the release paragraph as the five-step sequence and recovery policy of §4. Tags now exist and are immutable. Update the gates table to add the package matrix.
- **`README.md` Install**: add "Prebuilt packages (x86_64)", pointing to GitHub Releases. The `.deb` is "tested on Debian 12 and 13; Ubuntu 22.04, 24.04 and 26.04" (`sudo apt install ./tenuto_<ver>-1_amd64.deb`). The tarball lists the inspected runtime libraries and glibc version plus `ca-certificates`, with `sha256sum -c SHA256SUMS`. `cargo install tenuto` and the from-source steps stay.
- **`CHANGELOG.md`**: `[0.1.2]` and older stay unchanged. At the first tagged release (say `v0.1.3`): `[0.1.3]: …/compare/855fcb5...v0.1.3` and `[Unreleased]: …/compare/v0.1.3...HEAD`. Later entries compare two tags. No tags are backfilled for 0.1.0 or 0.1.2.
- The maintainer's memory note on the release pipeline is updated once this merges.

## 11. Files touched

- New: `.github/workflows/package.yml`; `scripts/release/{check-elf,smoke-test,check-tag,find-rehearsal,changelog-section,guard-release,build-artifacts,verify-deb-install,verify-tarball-install,test}.sh`; `scripts/release/lib.sh`; `scripts/release/fixtures/`; `scripts/release/tests/`.
- Changed: `.github/workflows/ci.yml` (rename `package` to `crate`, add `linux-packages` and `release-scripts`), `.github/workflows/release.yml` (rewritten), `Cargo.toml` (`[package.metadata.deb]`; `exclude` gains `/scripts`), `src/cli.rs`, `tests/cli.rs`, `docs/architecture.md`, `README.md`, `CHANGELOG.md` (an Unreleased "Added" entry for `--version` and the packages).
