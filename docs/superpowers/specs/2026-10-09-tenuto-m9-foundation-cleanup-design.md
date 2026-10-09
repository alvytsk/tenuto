# Tenuto M9: foundation cleanup

Status: approach approved in conversation on 2026-10-09; this written spec awaits review.

Branch: `refactor/m9-foundation-cleanup`, from `main` at `0053bfb`.

Context: PR B of the five that finish M9 ([layering test spec](2026-10-08-tenuto-m9-layering-test-design.md), its PR table). A shipped `tests/m9_layering.rs` with 18 allowlisted exceptions; this PR deletes the 12 marked `// B`.

## 1. Goal

Foundation modules stop importing from `playback`, `feed` and `http`, and `tui` stops importing from `cli`. Each moved item goes to the Foundation module that owns its meaning, every import is rewritten to the new path, and nothing is left behind at the old one.

## 2. Compatibility boundary

Unchanged, and verified (§5):
- CLI and TUI behavior: arguments, defaults, `--help` text, exit codes, every printed line.
- Persisted bytes: `state.json`, `subscriptions.json`, `stations.json` and the feed cache serialize exactly as today.
- Error text and error structure: every `Display` string, every `source()` chain, every `PlaybackError` variant a caller can match on today.
- Diagnostics: what `tests/m4_diagnostics.rs` audits.

Changed on purpose: Rust import paths and the signatures listed in §3. The library crate has no consumer outside this repository, so there are no compatibility shims: no `pub use` at an old path, no deprecated alias.

## 3. Moves

### 3.1 Value types out of `playback`

| Item | From | To |
|---|---|---|
| `PositionProvenance` | `playback/provenance.rs` | `media/provenance.rs` (file moved whole) |
| `PlaybackCheckpoint` | `playback/checkpoint.rs` | `resume.rs` (file deleted) |
| `Volume` | `playback/volume.rs` | `src/volume.rs`, a new top-level module (file moved whole) |

`PositionProvenance` says whether a media time can be trusted, so it belongs to `media`. `PlaybackCheckpoint` is the logical resume position and touches nothing in `persistence`, so it sits with the resume decision. `playback` never uses it. `Volume` is listener-set output gain. It is neither media metadata nor a resume value, so it gets its own module.

`src/lib.rs` declares `pub mod volume;`, and `tests/m9_layering.rs` gives `volume` Foundation rank (0) in `LAYERS`.

Derives, serde attributes, doc comments and unit tests move unchanged. `playback/mod.rs` loses `checkpoint`, `provenance` and `volume`.

### 3.2 The container probe out of `playback::decode`

The probe helpers `media::tags` borrows move from `playback/decode.rs` to a new `media/probe.rs`, unchanged apart from their error type: `LocalFile`, `open_local_file`, `ProbedContainer`, `probe_container`, `track_duration`, `StandardNames`, `standard_names`, and the private `year_of` with its unit test. Their visibility stays `pub(crate)`. `playback::decode` imports them from `media::probe`, which is a downward edge.

`media/probe.rs` defines the error these produce:

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("cannot open media {path:?}")]
    Open { path: PathBuf, #[source] source: std::io::Error },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("terminal I/O error")]
    Io(#[from] std::io::Error),
}
```

Each variant keeps the wording of the `PlaybackError` variant it stands for, including "terminal I/O error" for `Io`. That wording is not accurate for a file read, but changing it is out of scope (§7).

`playback/error.rs` adds `impl From<ProbeError> for PlaybackError`. It maps each variant to the same-named `PlaybackError` variant and moves its fields and `source` across as they are, so nothing is wrapped and nothing is re-rendered. `?` in `DecodedSource::open` and `DecodedSource::from_media_source` therefore still yields today's `PlaybackError` values.

Signatures that change:

| Item | Today | After |
|---|---|---|
| `media::vbr_header::probe_vbr_header` (and its private `gather_evidence`, `read_up_to`) | `Result<_, PlaybackError>` | `Result<_, ProbeError>` |
| `media::tags::probe_local_tags` | `Result<LocalTags, PlaybackError>` | `Result<LocalTags, ProbeError>` |
| `application::enrich::TagProbe` | `Fn(&AbsolutePath) -> Result<LocalTags, PlaybackError>` | `… -> Result<LocalTags, ProbeError>` |

`enrich` keeps rendering a failure with `error.to_string()` into `EnrichOutcome::Failed`, so that text is unchanged. `artwork::worker` keeps mapping any probe error to `ArtworkError::Missing`.

### 3.3 `AppError` to `app.rs`

`AppError` moves from `error.rs` to `app.rs` unchanged: the same three `transparent` arms, `Playback`, `Feed` and `Lifecycle`. `error.rs` keeps `DomainError`, `LifecycleError` and `TelemetryError` and no longer references `feed` or `playback`.

`tui` only ever puts a `LifecycleError` into `AppError`, so it returns that type directly:

| Item in `tui/mod.rs` | Today | After |
|---|---|---|
| `pub fn run` | `Result<RunOutcome, AppError>` | `Result<RunOutcome, LifecycleError>` |
| `Ending::Failed` | `Failed(AppError)` | `Failed(LifecycleError)` |
| `attempt!` | `Ending::Failed(AppError::from(error))` | `Ending::Failed(LifecycleError::from(error))` |
| the five `LifecycleError::Terminal(error).into()` sites | into `AppError` | the `LifecycleError` itself |
| `teardown` | `Result<RunOutcome, AppError>`; `Err(LifecycleError::WorkerPanicked.into())` | `Result<RunOutcome, LifecycleError>`; `Err(LifecycleError::WorkerPanicked)` |

`app::run` calls `tui::run` from two places, the bare `tenuto` path and `CliCommand::Tui`. Each converts with `.map_err(AppError::from)` (or `Ok(crate::tui::run(…)?)`). `main.rs` still receives an `AppError` whose `Lifecycle` arm is `transparent`, so the printed line and the logged `?error` chain do not change.

### 3.4 `redact_url` to `telemetry`

`redact_url` moves from `http/error.rs` to `telemetry.rs` unchanged. Its doc comment carries §7.2's rule. All 12 files that import it change to `crate::telemetry::redact_url` (or `tenuto::telemetry::redact_url` in `tests/`), and so does the intra-doc link in `tests/m4_diagnostics.rs`'s module doc. `http/error.rs` keeps no copy; its intra-doc link on `RemoteFailure` becomes [`crate::telemetry::redact_url`].

### 3.5 `MouseMode` and `ArtworkMode` to `tui`

`MouseMode` moves to `tui/mod.rs` beside `TuiOptions`. `ArtworkMode` moves to `tui/images.rs`, its main user, and `tui/mod.rs` imports it from there. Both keep `clap::ValueEnum`, `Default` and their doc comments, so there is no adapter type and `--help` text does not change. `cli.rs` and `app.rs` import them from `tui`, which is a downward edge.

## 4. Layering test

- `LAYERS` gains `("volume", 0)`.
- `ALLOWED` loses exactly the 12 entries marked `// B`. What remains is the 6 `// C` entries, unchanged. The stale-entry check proves each B edge is gone: if one were still there, its entry would not have gone stale and the test would not pass.
- No other change to the test.

## 5. Verification

Before any move, on the branch's first commit, the PR adds the pinning tests. Each must pass against today's code and then pass unchanged after every move:
- **Persisted bytes.** A test in `tests/persistence_model.rs` serializes a fixed `PersistedState` with `serde_json::to_string` and compares the whole string against a literal. The state carries a volume, an established checkpoint, an estimated-only entry, and a playlist whose entry has an `Estimated` duration provenance. The expected literal is captured from `main`.
- **CLI.** `tests/cli.rs` asserts that `tenuto tui` parses to `MouseMode::On` and `ArtworkMode::Auto`, that `MouseMode::default()` and `ArtworkMode::default()` are those same values (the bare `tenuto` path uses `default()`), and that `--mouse off --artwork blocks` parses. It also compares the `--mouse` and `--artwork` lines of `tui`'s rendered long help against literals captured from `main`.

Added with the move, as a unit test in `playback/error.rs`, a focused conversion test. For each `ProbeError` variant built with a known source, the converted `PlaybackError`:
- is the same-named variant;
- has the same fields;
- has the same `Display`;
- has a `source()` with the same `Display`, and for `Open` and `Io` the same `io::ErrorKind`.

`tests/m4_diagnostics.rs` changes only its import path and doc link and must pass. The full gate (fmt, clippy `-D warnings`, the whole suite, rustdoc `-D warnings`) must be green.

## 6. Documentation

- `architecture.md` §4:
  - In the diagram, `volume.rs` joins the `clock.rs, telemetry.rs, error.rs` Foundation node, and the `media/` node names provenance and the container probe.
  - In the component table, `media` adds provenance and the container probe, the `playlist`/`resume` row adds `PlaybackCheckpoint`, and `app` names `AppError`.
  - The "Foundation reaches up" bullet under Known layering exceptions is deleted.
- `architecture.md` §10 (diagnostics and errors): `AppError` lives in `app.rs`; `tui::run` returns `LifecycleError`.
- No CHANGELOG entry: nothing changes for users (§2).

## 7. Out of scope

- Correcting "terminal I/O error" for an `Io` failure that is not terminal I/O. That is a user-visible wording change, so it belongs in its own PR with a CHANGELOG line.
- The `// C` edges (`commands::displayable`, `wait_http`, `platform_*_store`). PR C removes them.
- Re-exports kept for older import paths, such as `session`'s re-export of `resume`'s items. They point downward and are not B's concern.
- Any change to `PlaybackError`'s variants or to `DomainError`, `LifecycleError` or `TelemetryError`.
