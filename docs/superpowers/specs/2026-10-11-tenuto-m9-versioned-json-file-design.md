# Tenuto M9: one versioned JSON file, and resume intent in `resume.rs`

Status: design approved in conversation on 2026-10-11; this written spec awaits review.

Branch: `refactor/m9-json-file`, from `main` at `1db0cea`.

Context: PR D of the five that finish M9 ([layering test spec](2026-10-08-tenuto-m9-layering-test-design.md), its PR table). C shipped as #43 and left `ALLOWED` empty. This PR carries out the two remaining M9.1 items of the [deepening spec](2026-09-29-tenuto-m9-architecture-deepening.md) §3: "One versioned JSON file module" and "Resume intent lives in `resume.rs`".

## 1. Goal

- One module, `persistence::json_file`, owns the lifecycle that `state.json`, `subscriptions.json` and `stations.json` each carry a copy of today: version-first decode, the not-found / unreadable / unsupported / malformed classification, quarantine with up to 100 candidates, the timestamp stamp, the serde-error sanitizer and the atomic save.
- `library::load_mutating` and `load_mutating_stations` become one function.
- `resume.rs` owns the fresh-load resume intent: the type, the function and its `None` → `StartAt(ZERO)` default. Its two helpers become private.

## 2. Compatibility boundary

Unchanged, and verified (§7):
- On-disk formats: `state.json` schema 4 (reads 1..=4, migrates below 4), `subscriptions.json` and `stations.json` schema 1. Pretty JSON, mode 0600, temp-write-fsync-rename-fsync-parent through `atomic::replace_bytes`.
- Recovery: an unreadable file and an unsupported version are preserved in place with writing off; a malformed file is quarantined as `<file name>.rejected-<stamp>[-n]` (n ≤ 100), writing on only if the move succeeded. A read-only path never quarantines, creates a directory, copies or writes.
- `state.json` queue recovery: the original bytes go to `state.json.queue-recovery-<stamp>[-n]` before any writer runs; a failed copy disables writing. `QueueRepair`, `QueueBackup`, `LoadOutcome` and `StateSnapshot` keep their shapes.
- Mutation refusal: only a writable `Loaded` or `Missing` load lets a subscription or station mutation proceed. A successful quarantine still refuses the current mutation.
- Read-only errors: `SubscriptionStore::read_snapshot` and `StationStore::read_snapshot` return the same `FeedError::{Subscriptions,Stations}Unreadable` reason text as today; `StateStore::read_snapshot` the same `PersistenceError` variants.
- Resume: every `MediaId` × stored-entry combination yields the same `ResumeIntent` as today's `Session::resume_intent`. Completion still wins over an estimate: a completed entry gives `Candidate { completed: true, .. }` even with no established position. Only today's `None` paths become `StartAt(ZERO)`.
- Logging: no log line carries file content or an unredacted URL (`tests/m4_diagnostics.rs`). A malformed file's log still explains why: the sanitized category, line and column for a JSON error, the static validation reason for a record (§3).

Changed on purpose:
- The mutation-refusal text for both files comes from one template (§4.3). Neither the old nor the new text is pinned by a test or `reference.md`.
- A serialize failure in `SubscriptionStore::save` or `StationStore::save` arrives as `FeedError::Persistence(PersistenceError::Serialize { .. })` through the existing `#[from]`, not as an `…Unreadable` reason. It is reachable: record validation does not check RFC3339 representability, so a public snapshot whose `added_at` (or a station's `probed_at`) has a negative year passes validation and fails the serializer. The destination file is left byte-identical (§6.2).
- `StateStore::read_snapshot` no longer emits its own decode `warn!`; the same sanitized detail reaches its caller inside `PersistenceError::Deserialize`, as it does today.
- The lifecycle's `tracing::warn!` lines move into `json_file` and name the file through a `file` field; their message text is shared across the three files.
- Rust paths: `ResumeIntent` moves to `resume` (re-exported from `playback::command`); `session::resume_intent_for`, `resume::resume_candidate`, `resume::restart_preference` and `resume::RestartPreference` leave the public surface. `CHANGELOG.md` records the break under `### Internal` (§6).

## 3. `persistence::json_file`

`pub(crate)`, rank 0, beside `atomic.rs`. A concrete struct with generic methods, not a trait (`architecture.md` §12).

```rust
pub struct JsonFile { path: PathBuf, clock: Arc<dyn Clock> }

pub enum LoadReason { Loaded, Missing, Quarantined { moved_to: PathBuf }, QuarantineFailed,
                      UnsupportedVersion { found: u32 }, Unreadable }   // moved from store.rs, unchanged
pub enum Malformed { Json { category: &'static str, line: usize, column: usize }, Invalid(&'static str) }
pub enum Rejected { Unreadable(io::Error), UnsupportedVersion(u32), Malformed(Malformed) }

pub struct Load<T> { pub found: Option<Found<T>>, pub writable: bool, pub reason: LoadReason }
pub struct Found<T> { pub value: T, pub bytes: Vec<u8> }

pub const MAX_QUARANTINE_CANDIDATES: u32 = 100;

impl JsonFile {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self;
    pub fn path(&self) -> &Path;
    pub fn now(&self) -> OffsetDateTime;
    pub fn read<T>(&self, versions: RangeInclusive<u32>,
                   decode: impl FnOnce(u32, &[u8]) -> Result<T, Malformed>) -> Result<Option<T>, Rejected>;
    pub fn load<T>(&self, versions: RangeInclusive<u32>,
                   decode: impl FnOnce(u32, &[u8]) -> Result<T, Malformed>) -> Load<T>;
    pub fn save(&self, value: &impl Serialize) -> Result<(), PersistenceError>;
    pub fn keep_copy(&self, bytes: &[u8], tag: &str) -> Option<PathBuf>;
}

pub fn malformed(source: &serde_json::Error) -> Malformed;
pub(crate) fn stamp(at: OffsetDateTime) -> String;
```

Contract:
- Both `read` and `load` read the bytes, then deserialize only `{ schema_version: u32 }`. A version outside `versions` is `UnsupportedVersion` before the full shape is looked at, so a newer file with an alien shape is never called garbage. An envelope that does not deserialize is `Malformed::Json`. Otherwise `decode(found_version, bytes)` runs; the version lets `state.json` migrate.
- `read`: `NotFound` → `Ok(None)`. Any other read error → `Rejected::Unreadable`. Never quarantines, creates a directory, copies or writes.
- `load`: `NotFound` → `found: None`, writable, `Missing`. Read error → `found: None`, not writable, `Unreadable`, file untouched. Unsupported version → `found: None`, not writable, `UnsupportedVersion { found }`, file untouched. Malformed → quarantine; `Quarantined { moved_to }` and writable if the rename succeeded, else `QuarantineFailed` and not writable; `found: None` either way. Success → `found: Some(Found { value, bytes })`, writable, `Loaded`, where `bytes` are exactly the bytes `decode` saw.
- Quarantine: candidates `<file name>.rejected-<stamp>`, then `-2` … `-100`; an existing candidate is skipped, never overwritten; the first failed rename gives up. Single-user, single-process, so the exists-then-rename window stays acceptable (today's comment moves with it).
- `save`: `serde_json::to_vec_pretty`, then `atomic::replace_bytes(path, bytes)`. A serialize failure is `PersistenceError::Serialize { path, source }`; I/O failures keep `replace_bytes`'s `Io { path, op }` context. `save` validates nothing: each store validates before it calls `save`.
- `keep_copy`: candidates `<file name>.<tag>-<stamp>[-n]` (n ≤ 100), each opened create-new through `atomic::create_private_new`; `AlreadyExists` moves to the next candidate. Then `atomic::set_private` explicitly (the umask can alter the create mode), `write_all`, `sync_all`, and `atomic::sync_parent_best_effort`. Returns `Some(path)` on success and `None` on any failure except the parent sync, which stays best-effort.
- `malformed`: the one sanitizer. Keeps `classify()`'s category, line and column; drops the `Display` text, which can quote file content.
- `stamp`: `20260908T143211Z`, moved from `store.rs`. `lifecycle::stderr` imports it from here; the copy in `subscription/store.rs` and `station/store.rs` is deleted.
- Logging: `load` emits today's `warn!`s — unreadable (with the `io::Error`), unsupported (`found`, `supported`), malformed, and quarantined / quarantine failed (`moved_to`) — with a `path` field and no content. The malformed line carries the sanitized detail as fields: `category`, `line`, `column` for `Malformed::Json`, `reason` for `Malformed::Invalid`. `read` logs nothing: it returns the same detail in `Rejected`, and each caller surfaces it.

`persistence::store` re-exports `LoadReason` and `MAX_QUARANTINE_CANDIDATES`, so `persistence::store::LoadReason` keeps working for `application::profile`, `library` and the two stores. `persistence::mod` declares `pub(crate) mod json_file`.

## 4. The three stores on top

### 4.1 `StateStore`

Public API unchanged: `new`, `path`, `platform_path`, `load`, `read_snapshot`, `write`. It holds a `JsonFile` and no path or clock of its own.

- `decode_state(path, version, bytes, accept_queue) -> Result<(PersistedState, Option<QueueReset>), Malformed>` stays in `store.rs` without the envelope step; `path` stays for the migration `info!`. `RawState` deserialize errors go through `json_file::malformed`. When `version != SCHEMA_VERSION` it calls `migrate_to_current_schema` as today.
- `load`: `file.load(OLDEST_MIGRATABLE_VERSION..=SCHEMA_VERSION, |v, b| decode_state(path, v, b, true))`.
  - `found: Some` with `Some(reset)`: `file.keep_copy(&found.bytes, "queue-recovery")`; `Some(p)` → `QueueBackup::Saved(p)`, `None` → `QueueBackup::Failed` and `writable = false`; the existing `warn!` with `fields_reset()` stays here.
  - `found: Some` with `None`: today's `Loaded` outcome.
  - `found: None`: `PersistedState::default()` with the reason and writable flag `JsonFile` reports.
- `read_snapshot`: `file.read(…, accept_queue = false)`. `Ok(None)` → empty snapshot. `Rejected::Unreadable(e)` → `PersistenceError::Io { path, op: "read", source: e }`. `UnsupportedVersion(found)` → `PersistenceError::UnsupportedVersion { path, found, supported: SCHEMA_VERSION }`. `Malformed::Json { .. }` → `PersistenceError::Deserialize { path, category, line, column }`. `Malformed::Invalid(what)` → `Deserialize { path, category: what, line: 0, column: 0 }`: unreachable, since state decode never builds `Invalid`; kept as an exhaustive fallback with a comment.
- `write`: keeps its `debug_assert_eq!` on the schema version, then `file.save(state)`.
- `back_up_original`, `quarantine`, `reject_malformed`, `fresh`, `parent`, `VersionEnvelope` and the local `malformed` are deleted.

### 4.2 `SubscriptionStore` and `StationStore`

Public API unchanged (`new`, `path`, `now`, `read_snapshot`, `load`, `save`, and `StationStore::logo_for` / `is_station`). Each holds a `JsonFile`; `now` delegates to `JsonFile::now`.

- `decode_subscriptions` / `decode_stations` become `decode(version, bytes) -> Result<Snapshot, Malformed>` over the full DTO and `validate_records`. `versions` is `SCHEMA_VERSION..=SCHEMA_VERSION`. Every validation failure is `Malformed::Invalid` with today's static text ("a slug is not valid", …).
- `read_snapshot` maps `Rejected` to today's exact reason strings:
  - `Unreadable(e)` → `"cannot read <noun> file: {e}"`;
  - `UnsupportedVersion(found)` → `"<noun> file is schema version {found}, and this build supports {SCHEMA_VERSION}"`;
  - `Malformed::Json { category, line, column }` → `"<noun> file is malformed ({category} error at line {line}, column {column})"`;
  - `Malformed::Invalid(what)` → `what`.
- `load` maps `Load<Snapshot>` to `SubscriptionLoad` / `StationLoad`, an empty snapshot when `found` is `None`.
- `save` keeps its validation over the records first (mapped to `…Unreadable` as today), builds the file DTO, then `file.save(&dto)?`.
- The local `DecodeError`, `VersionEnvelope`, `malformed`, `quarantine`, `reject_malformed`, `parent`, `fresh` and `stamp` are deleted.

### 4.3 `library`

One helper replaces `load_mutating` and `load_mutating_stations`:

```rust
fn writable<T>(snapshot: T, writable: bool, reason: LoadReason,
               file: &str, consequence: &str) -> Result<T, String>
```

`Ok(snapshot)` only when `writable` and `reason` is `Loaded` or `Missing`. Otherwise the refusal text:

| `LoadReason` | Text |
|---|---|
| `Quarantined { moved_to }` | `{file} was malformed and has been moved aside to {moved_to}; {consequence}` |
| `QuarantineFailed` | `{file} is malformed and could not be moved aside; {consequence}` |
| `Unreadable` | `{file} could not be read; {consequence}` |
| `UnsupportedVersion { found }` | `{file} is schema version {found}, which this build does not support; {consequence}` |
| `Loaded` / `Missing` (not writable; unreachable) | `{file} is not writable; {consequence}` |

The match stays exhaustive, with no wildcard. Callers pass `"subscriptions file", "subscribe, unsubscribe and refresh are unavailable"` and wrap in `FeedError::SubscriptionsUnreadable`, or `"stations file", "no station was changed"` and wrap in `FeedError::StationsUnreadable`.

## 5. `resume.rs`

`resume` is rank 0, and `NAMED_RULES` forbids `resume → persistence`, so the function takes a persistence-free view:

```rust
pub enum ResumeIntent { StartAt(Duration), Candidate(ResumeCandidate),
                        EstimatedCandidate { target: Duration, established: Option<Duration> } }
pub struct StoredProgress { pub position: Option<Duration>, pub estimated: Option<Duration>, pub completed: bool }
pub fn resume_intent(media: &MediaId, stored: Option<StoredProgress>) -> ResumeIntent;
```

- `ResumeIntent` moves from `playback::command` with its docs unchanged; `playback::command` keeps `pub use crate::resume::ResumeIntent;`.
- `resume_intent` is today's `session::resume_intent_for` body, made total:
  1. `LocalFile` or `RemoteUrl` media → `StartAt(ZERO)` (M8 §7: no resume on a fresh load).
  2. No stored progress → `StartAt(ZERO)`.
  3. `completed` → `Candidate(resume_candidate(position, true))`, which always exists: completion wins over any estimate.
  4. An estimate → `EstimatedCandidate { target: estimated, established: position }`.
  5. Otherwise `resume_candidate(position, false)` → `Candidate`, or `StartAt(ZERO)` when there is no position.
- `resume_candidate`, `restart_preference` and `RestartPreference` become private. `decide_resume`, `ResumeDecision`, `ResumeCandidate`, `KnownDuration` and `PlaybackCheckpoint` stay public (the engine uses them).
- `Session::resume_intent(&self, media)` becomes `resume::resume_intent(media, self.state.entry_for(media).map(|e| StoredProgress { position: e.position, estimated: e.estimated, completed: e.completed }))`. `session::resume_intent_for` is deleted; `session`'s `pub use crate::resume::{ResumeDecision, decide_resume}` stays.
- Rustdoc that links the privatized helpers or the deleted function is rewritten in `resume.rs`, `playback/command.rs`, `playback/engine.rs`, `session.rs` and `application/profile.rs`, including the stale "`src/app.rs`'s `resume_intent_for`".

## 6. Tests and documentation

### 6.1 The lifecycle suite

`#[cfg(test)] mod tests` in `persistence/json_file.rs` (the module is crate-private), on a tempdir with a fixed clock and a toy `{ schema_version, items: [u32] }` format:
- `read`: missing → `Ok(None)` and no parent directory created; a directory at the path → `Unreadable`; a version above and below the band → `UnsupportedVersion`, the envelope winning over an alien shape; a syntax error → `Malformed::Json`; a decode returning `Invalid` → `Malformed::Invalid`. Every case leaves the directory listing unchanged.
- `load`: missing → `Missing`, writable; unreadable and unsupported → preserved, not writable; malformed (syntax and `Invalid`) → quarantined as `<name>.rejected-<stamp>`, writable; a collision → `-2`; all 100 candidates taken → `QuarantineFailed`, not writable, file kept; `decode` receives the found version; `Found::bytes` equal the file's bytes.
- `save`: pretty JSON that `read` decodes back, no temp file left, mode 0600.
- `keep_copy`: exact bytes, mode 0600, a collision → `-2`, never overwrites.
- `malformed`: a file seeded with a URL yields a `Malformed` whose `Debug` does not contain it.

### 6.2 The per-store suites

Record validation plus one wiring test each. The wiring test pins the quarantine name prefix and the version band, so a wrapper passing the wrong file name or versions fails.
- `m4_subscription_store`: keeps the feed id, slug, URL and timestamp validation tests, the round-trip and 513-entry tests, and `only_mutating_load_quarantines` as the wiring test. The collision, every-candidate-taken, unsupported-version, directory-at-path and missing-creates-no-directory tests move to §6.1.
- `m7_1_station_store`: keeps the identity round-trips, media/url mismatch, duplicate slug, `logo_for`, `is_station` and slug derivation; `read_snapshot_reports_a_malformed_file…` and `load_quarantines_a_malformed_file…` become one wiring test.
- `persistence_store`: keeps the v1 band and migration tests (`a_v1_file…`, `the_upgrade_cycle…`, `a_version_one_file_that_will_not_deserialize…`, `a_genuinely_unknown_version…`), the privacy tests, and one wiring test; the missing, quarantine, collision, quarantine-failure, unsupported and alien-newer tests move to §6.1.
- `m4_subscription_store` and `m7_1_station_store` each gain one save test: a pre-existing valid file, then `save` of a snapshot whose `added_at` has year −1 returns `FeedError::Persistence(PersistenceError::Serialize { .. })`, and the file's bytes are unchanged.
- `m5_state_recovery`, `m8_state_playlists` and the other queue-recovery suites stay unchanged: they pin `keep_copy`'s use and `writable = false` on a failed backup.

### 6.3 The resume suites

Split by what a test pins, so engine-decision tests never pick up the fresh-load media policy:
- Stored-progress selection goes through `resume::resume_intent` with a `MediaId::PodcastEpisode`, the one kind that resumes on a fresh load: `resume_decision`'s `resume_candidate_*` and `restart_preference_*` tests, `resume_contract.rs:575/609`, `estimated_resume.rs:288/437`. `resume_decision` gains the cases that today live only behind `session`: local file and URL → `StartAt(ZERO)`; `None` → `StartAt(ZERO)`; completed with no position → `Candidate { completed: true }`; completed with an estimate → `Candidate`, not `EstimatedCandidate`.
- Engine resume decisions build the candidate literally and keep their media: `ResumeCandidate { position: entry.position.expect(…), completed: entry.completed }` in `http_resume`, `http_playback`, `estimated_resume.rs:231/556`, `resume_contract`'s `decide_resume` calls and `session_policy.rs:703`. A one-line comment says the media is deliberately one the fresh-load policy would not resume.
- `m5_session_queue`, `m8_resume` and `application::profile`'s tests keep calling `Session::resume_intent`.

### 6.4 Documentation

- `architecture.md` §4: the `persistence` row gains "one versioned JSON file lifecycle — read, load for writing, quarantine, save — shared by `state.json`, `subscriptions.json` and `stations.json`"; the `station` row's "the subscription store's read, recovery and quarantine policy" becomes "the shared file lifecycle"; the `playlist` / `resume` row adds that `resume` owns the fresh-load resume intent (`resume_intent`).
- The deepening spec §3: a Status line under both items naming this PR.
- `reference.md`: no change.
- `CHANGELOG.md` `[Unreleased]`: `### Changed` — when `subscriptions.json` or `stations.json` cannot be written because it was malformed, unreadable or from a newer build, the refusal reads the same way for both files. `### Internal` — the three state files share one read, recovery and save path; `ResumeIntent` moves to `resume`, and the resume helpers leave the public surface.
- `docs/m9-d-acceptance.md`, following `m9-c-acceptance.md`: suite count, the CLI parity check, `ALLOWED` still empty.

## 7. Verification

- The full gate in `CLAUDE.md`: `fmt --check`, `clippy -D warnings`, `cargo test --locked --no-fail-fast`, `RUSTDOCFLAGS="-D warnings" cargo doc`.
- `tests/m9_layering.rs` passes with `ALLOWED` empty and no new `NAMED_RULES` exception.
- `tests/m4_diagnostics.rs` passes, and gains one subprocess test (default `tenuto=info` filter): `unsubscribe` against a `subscriptions.json` with a syntax error, then against one whose only fault is an invalid slug, then `play` against a `state.json` with a syntax error. stderr shows the `category`/`line`/`column` fields for the two syntax errors and the `reason` field for the slug, and never the seeded `https://SECRETHOST/…` URL the files carry.
- CLI parity against `main`: a script builds both binaries and runs each over its own copy of the same seeded scratch `XDG_*` profile. Cases: `feeds`, `episodes`, `subscribe` / `unsubscribe` against a malformed, an unsupported-version, an unreadable (directory) and a valid `subscriptions.json`; `play` with `TENUTO_AUDIO_OUTPUT=null` against the same four `state.json` cases and a queue-repair one. Compared per case:
  - exit status, exactly;
  - stdout and stderr, after normalizing the scratch root to `<root>`, RFC3339 and log timestamps to `<ts>`, file-name stamps (`\d{8}T\d{6}Z`) to `<stamp>`, and generated feed IDs to `<feed-N>` in order of first appearance;
  - the profile's resulting file tree: the same names after the same normalization; written JSON compared as parsed values after normalizing `feed_id`, `added_at` and `updated_at`; every preserved original — an unreadable or unsupported file left in place, a `*.rejected-*` quarantine, a `*.queue-recovery-*` copy — byte-identical to the seeded bytes.
  - Allowed differences, and only these: the §4.3 refusal text on stderr; the lifecycle `warn!` message text (§3), provided each line keeps its fields. Any other diff fails the check.
- Over `src/persistence/`, `src/subscription/store.rs` and `src/station/store.rs` — not `src/feed/cache.rs`, whose own `VersionEnvelope` (with `parser_version`) stays under §8 — a `grep` for `fn stamp`, `struct VersionEnvelope`, `MAX_QUARANTINE_CANDIDATES` definitions and `rejected-` format strings finds one each, all in `json_file.rs`.

## 8. Out of scope

- The feed cache (`feeds/<feed-id>.json`): disposable, refetchable, no quarantine. It does not share this lifecycle.
- Any schema bump, new field or format change.
- A lock change: `state.lock` and `subscriptions.lock` stay where they are.
- M9.4 (PR E).
