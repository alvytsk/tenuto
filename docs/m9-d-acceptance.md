# M9 PR D: one versioned JSON file, and resume intent in `resume.rs` — acceptance

Scope: the M9.1 "One versioned JSON file module" and "Resume intent lives in
`resume.rs`" items of
`docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §3.
Spec: `docs/superpowers/specs/2026-10-11-tenuto-m9-versioned-json-file-design.md`.
Plan: `docs/superpowers/plans/2026-10-11-m9-versioned-json-file.md`.

Branch `refactor/m9-json-file`, from `main` at `1db0cea`.

## Decisions

Made with the user on 2026-10-11:

- `persistence::json_file` is a crate-private concrete struct with generic
  `read`, `load`, `save` and `keep_copy`, not a trait. `LoadReason` and
  `MAX_QUARANTINE_CANDIDATES` keep their `persistence::store` paths through
  re-exports.
- No new error type for saving: `save` returns `PersistenceError` through
  `atomic::replace_bytes`. A store's serialize failure now arrives as
  `FeedError::Persistence(Serialize)`.
- `read` never quarantines, creates a directory, copies or writes; `load`
  preserves unreadable and unsupported files and quarantines malformed ones.
  `keep_copy`'s parent fsync stays best-effort.
- A malformed file's log keeps its sanitized detail as fields: `category`,
  `line`, `column` for JSON, `reason` for a validation sentence.
- `library`'s two load-for-writing refusals become one `writable` helper
  with one template; the store passes its noun and consequence.
- `resume_candidate`, `restart_preference` and `RestartPreference` become
  private. `ResumeIntent` moves to `resume` (re-exported from
  `playback::command`); `resume_intent` takes a persistence-free
  `StoredProgress`.
- Resume tests split by what they pin: stored-progress selection goes
  through `resume_intent` on a podcast episode; the engine's own decision
  gets a literal candidate (`support::established_candidate`) and keeps its
  local or URL fixture.

## Gate

Run on this branch after the last code change.

| Check | Command | Result |
|---|---|---|
| Format | `cargo fmt --check` | exit 0, no output |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | no issues |
| Tests | `cargo test --locked --no-fail-fast` | exit 0; 112 result lines: 1484 passed, 0 failed, 3 ignored |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | no warnings |

The 3 ignored are the intentional ones: `device_smoke` and the two
`m8_snapshot_size` measurements. `main` at `1db0cea` ran 1472 passed; the
net +12 is 32 added and 20 deleted.

- Added: 14 lifecycle tests in `persistence::json_file`;
  `a_read_never_makes_a_recovery_copy` (`m5_state_recovery`);
  `read_errors_keep_their_text` and
  `a_save_that_cannot_serialize_leaves_the_file_as_it_was` in both store
  suites; the station wiring test `the_store_wires_its_file_name_and_version`
  and the rewritten `only_mutating_load_quarantines`; two
  `library::refusal_tests`; `a_malformed_file_logs_why_without_its_content`
  (`m4_diagnostics`); 8 `resume_intent_*` tests (`resume_decision`).
- Deleted, now owned by the lifecycle suite or `resume_intent`: 5 from
  `persistence_store`, 6 from `m4_subscription_store`, 2 from
  `m7_1_station_store`, 7 `resume_candidate_*` / `restart_preference_*`
  from `resume_decision`.
- `a_malformed_file_logs_why_without_its_content` was checked against a
  mutation: with `category`, `line` and `column` dropped from the log line it
  fails.

## Layering

`tests/m9_layering.rs` passes with `ALLOWED` empty and `NAMED_RULES`
unchanged: `resume` imports only `media`.

## CLI parity

`main` at `1db0cea` against the branch at `00946bb`, both release builds.
Each side ran over its own copy of the same seeded `XDG_*` profile per case;
one local `python3 -m http.server` served `rss2-minimal.xml` to both. stdout,
stderr, exit statuses and the resulting file trees were compared after
normalizing the profile root, timestamps, file-name stamps and generated
feed IDs; written JSON was compared parsed.

| Case | Commands | Result |
|---|---|---|
| `subscriptions.json` valid / missing | `feeds`, `subscribe <url> --as alpha`, `episodes alpha`, `unsubscribe alpha` | identical |
| `subscriptions.json` malformed (`broken`) | same | identical stdout, exit codes and tree; quarantine byte-identical to the seed; stderr differs as allowed below |
| `subscriptions.json` unsupported (`schema_version` 2) | same | identical but for the `warn!` line; file left in place byte-identical |
| `subscriptions.json` unreadable (a directory) | same | identical but for the `warn!` line; directory left in place |
| `state.json` valid / missing | `play sine.wav` (`TENUTO_AUDIO_OUTPUT=null`) | identical |
| `state.json` malformed | same | identical but for the `warn!` lines; quarantine byte-identical |
| `state.json` unsupported (`schema_version` 5) | same | identical but for the `warn!` line; file left in place |
| `state.json` unreadable (a directory) | same | identical but for the `warn!` line |
| `state.json` queue repair (schema 3, 5,000 entries) | same | identical, stderr included; `queue-recovery` copy byte-identical |

Every difference, all on stderr, is one spec §7 allows:

- The lifecycle `warn!` lines now come from
  `tenuto::persistence::json_file` with shared wording, each keeping its
  fields. For example `unsupported state schema; preserving the file and not
  writing this session path=… found=5 supported=4` became `unsupported
  schema; preserving the file and not writing this session path=… found=5
  supported=4`. A malformed `subscriptions.json` logged its detail as
  `reason="subscriptions file is malformed (syntax error at line 1, column
  1)"` and now logs it as `category="syntax" line=1 column=1`.
- The refusal after a quarantine lost its last words: `…; subscribe,
  unsubscribe and refresh are unavailable until this is resolved` is now
  `…; subscribe, unsubscribe and refresh are unavailable`. The unreadable
  and unsupported refusals came out word for word the same, and the
  read-only `feeds` errors were identical in every case.

## Scan

```
$ grep -rn "fn stamp\|struct VersionEnvelope\|const MAX_QUARANTINE_CANDIDATES\|rejected-" \
    src/persistence src/subscription/store.rs src/station/store.rs
src/persistence/json_file.rs:34:pub const MAX_QUARANTINE_CANDIDATES: u32 = 100;
src/persistence/json_file.rs:126:struct VersionEnvelope {
src/persistence/json_file.rs:369:pub(crate) fn stamp(at: OffsetDateTime) -> String {
```

plus `rejected-` five times in `json_file.rs`'s own tests. The quarantine
name is built once, in `JsonFile::candidates`. `src/feed/cache.rs` keeps its
own `VersionEnvelope` (with `parser_version`) by design and is outside the
scan.
