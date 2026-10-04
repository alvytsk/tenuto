# tenuto

Keyboard-first terminal audio player: one Rust binary, `play` CLI + ratatui `tui`.
Before touching playback, session or persistence code, read `docs/architecture.md`
§1 (invariants) and §4–5 (layers, thread ownership). User-facing rules: `docs/reference.md`.

## Commands

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast        # plain `cargo test` stops at the first failing binary
cargo test --locked --test m8_playlist    # one suite
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
bash scripts/release/test.sh              # release-script tests (stubbed gh/curl)
TENUTO_AUDIO_OUTPUT=null cargo run --locked -- tui   # paced virtual output, no sound device
```

Toolchain pinned to 1.98.1 (`rust-toolchain.toml`); every cargo command uses `--locked`.
Linux builds need `libasound2-dev`.

## Rules reviewers enforce

- Runtime code: no `unsafe`, `unwrap`, `expect` (lints deny; tests may use them via `clippy.toml`).
- Every URL in a message or log goes through `redact_url`; never log a parsed feed item or document whole (`tests/m4_diagnostics.rs` audits this).
- Nothing plays, fetches or refreshes on its own — every network request follows a user action.
- Stopping or recreating the transport must never reset the logical position.
- Layering: `playback` never imports persistence or blocks on Tokio; `library` never prints or `block_on`s; only `session` builds `PersistedState` snapshots; `tui` mutates state only through `Session`.

## Tests

- Integration suites in `tests/`, prefixed by milestone (`m5_*`, `m8_*`); shared harness in `tests/support/`.
- Engine tests run on a virtual clock. Step it with `play_for`, `let_time_pass`, `play_until_terminal` — never a sleep or a fixed-duration advance; those have caused CI-only flakes.
- Runtime suites (`tests/support/runtime.rs`) share one virtual clock too: wait with `pump_until`, `pump_for`, or `step` in a test's own loop — never `runtime.pump()` plus a sleep.
- Network budgets and reconnect backoff: `TestEngine::start_on_fake_clock` holds network time; step it with `advance_network`, let it follow real time with `run_network`. Prefer it to widening a real backoff.
- Subprocess suites are Linux-only. The macOS CI leg is non-gating.
- `TENUTO_TEST_HOOK` triggers fixed-stage panics/probes (`src/lifecycle/hooks.rs`).
- Intentionally `#[ignore]`d: `device_smoke` (real device), `m8_snapshot_size` (manual measurement).
- A failure that only shows under load: pin the test binary with `taskset -c 0-1` beside ~8 `yes` loops; suspect a send-then-signal race in the submitting handle before blaming the runner.

## Workflow

- Feature work: spec in `docs/superpowers/specs/`, plan in `docs/superpowers/plans/`, acceptance record `docs/mN-acceptance.md`; update `architecture.md` / `reference.md` and `CHANGELOG.md` `[Unreleased]` (Keep a Changelog, hard-wrapped).
- Releases follow `docs/architecture.md` §11: release PR → `release.yml` rehearsal dispatch on that commit → `vX.Y.Z` tag → crates.io publish last, behind the `crates-io` environment approval. Never push a tag or run `cargo publish` without the user's explicit go.
