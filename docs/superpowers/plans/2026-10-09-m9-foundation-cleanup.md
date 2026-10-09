# M9 Foundation Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete the 12 `// B` entries from `tests/m9_layering.rs`'s `ALLOWED` by moving each item to the Foundation (or `tui`) module that owns it, with no change to behavior, persisted bytes or error text.

**Architecture:** Five moves, one task each. Every task deletes its own allowlist entries first (RED: the layering test names the edges), then moves the item and rewrites every import (GREEN). Task 1 pins `state.json`'s bytes and `tui`'s flags before anything moves. No shims are left at the old paths.

**Tech Stack:** Rust 1.98.1, edition 2024, thiserror 2, clap 4 (derive), symphonia 0.6.1.

**Spec:** `docs/superpowers/specs/2026-10-09-tenuto-m9-foundation-cleanup-design.md`. While this plan was written, Task 1's two tests were compiled and run against `main` at `0053bfb`: both pass, and clippy is clean. Deleting all 12 B entries and adding `("volume", 0)` produced exactly the RED lines quoted in Tasks 2–6.

## Global Constraints

- Every cargo command uses `--locked`; toolchain 1.98.1.
- Runtime code has no `unsafe`, `unwrap` or `expect` (the lints deny them). Tests may use them (`clippy.toml`).
- Compatibility (spec §2): CLI/TUI behavior, `--help` text, exit codes, persisted bytes, every error's `Display` and `source()` chain, and every `PlaybackError` variant stay as they are. Rust import paths and the signatures listed in spec §3 change. **No `pub use` shims** at old paths.
- Moved items keep their derives, serde attributes, doc comments and unit tests.
- Never add to `ALLOWED`. Each task deletes exactly the entries named in it.
- No CHANGELOG entry. Commits carry no `Co-authored-by` or tool attribution.
- Work on branch `refactor/m9-foundation-cleanup`, which already holds the spec commits.
- The gate at each task's end is `cargo fmt --check`, then `cargo clippy --locked --all-targets --all-features -- -D warnings`, then `cargo test --locked --no-fail-fast`.

## Review Focus

1. **An `Io` probe failure reaching the metadata column or the log.** `EnrichOutcome::Failed(error.to_string())` must still read "terminal I/O error", and a failure inside playback's `DecodedSource::open` must still surface as `PlaybackError::Io` with the same source. *Pinned in Task 3:* `a_probe_error_becomes_the_same_playback_error`.
2. **A state file written before the move, read after it.** The bytes must be identical, including `decoded_estimated` and the `estimated` field. *Pinned in Task 1:* `a_full_state_serializes_to_exactly_these_bytes`.
3. **A bare `tenuto` against `tenuto tui` with no flags.** Both must start with mouse on and artwork auto, with `--help` unchanged. *Pinned in Task 1:* `the_tui_flags_keep_their_defaults_and_help`.
4. **A TUI startup failure, such as the profile being locked by another instance.** The printed `tenuto: …` line and the exit status must not change once `tui::run` returns `LifecycleError`. *Pinned by the existing suite:* `tests/m5_tui_process.rs` (`tui_refuses_a_held_profile_before_entering_raw_mode`, `an_uncontained_worker_panic_restores_the_terminal_and_fails`) and `tests/m5_lock_process.rs` (subprocess, Linux). Task 4 runs them by name.
5. **A diagnostic carrying a URL.** It must still be redacted after `redact_url` moves. *Pinned by the existing suite:* `tests/m4_diagnostics.rs` and `tests/http_errors.rs`. Task 5 runs them by name.

---

### Task 1: Pin the persisted bytes and the `tui` flags

**Files:**
- Modify: `tests/persistence_model.rs` (append one test)
- Modify: `tests/cli.rs` (imports; append one test)

**Interfaces:**
- Consumes: nothing new.
- Produces: `a_full_state_serializes_to_exactly_these_bytes` and `the_tui_flags_keep_their_defaults_and_help`. Later tasks change only their import paths and never their literals.

These are characterization tests: they pin today's output, so they **pass on first run**. That is the point. If one fails, the literal was mistyped. Fix the literal against what `main` prints, never the code.

- [ ] **Step 1: Append the state test to `tests/persistence_model.rs`**

```rust
/// M9 foundation cleanup §5: the whole serialized form, byte for byte. A
/// volume, an established checkpoint, an estimated-only entry, and playlist
/// entries with both decoded duration provenances. Moving the types these
/// are built from must not change one character of `state.json`.
#[test]
fn a_full_state_serializes_to_exactly_these_bytes() {
    const STATE: &str = concat!(
        r#"{"schema_version":4,"current_media":"local:/music/a.flac","volume":0.5,"#,
        r#""checkpoints":{"#,
        r#""local:/music/a.flac":{"position":{"secs":93,"nanos":0},"completed":false,"#,
        r#""touch_seq":1,"updated_at":"1970-01-01T00:00:00Z"},"#,
        r#""local:/music/b.flac":{"position":null,"completed":false,"#,
        r#""touch_seq":2,"updated_at":"1970-01-01T00:00:00Z","estimated":{"secs":45,"nanos":0}}},"#,
        r#""playlists":[{"id":1,"name":"Default","shuffle":null,"entries":["#,
        r#"{"id":1,"media":"local:/music/a.flac","source":{"kind":"local","path":"/music/a.flac"},"#,
        r#""display":{"title":"A","artist":null,"album":null,"year":null,"#,
        r#""duration_ms":600000,"duration_source":"decoded_estimated"}},"#,
        r#"{"id":2,"media":"local:/music/b.flac","source":{"kind":"local","path":"/music/b.flac"},"#,
        r#""display":{"title":null,"artist":null,"album":null,"year":null,"#,
        r#""duration_ms":1000,"duration_source":"decoded"}}],"active_entry":1}],"#,
        r#""playing":1,"next_entry_id":3,"next_playlist_id":2}"#,
    );
    let state: PersistedState = serde_json::from_str(STATE).unwrap();
    assert_eq!(state.volume(), Volume::new(0.5));
    assert_eq!(serde_json::to_string(&state).unwrap(), STATE);
}
```

- [ ] **Step 2: Add the CLI test to `tests/cli.rs`**

Change the two imports at the top:

```rust
use clap::{CommandFactory, Parser};
use tenuto::cli::{ArtworkMode, Cli, CliCommand, MouseMode};
```

Append the test. The help text is spelled line by line with `\n` because two of its lines are exactly ten spaces, which an editor would strip from a multi-line literal:

```rust
/// M9 foundation cleanup §5: `MouseMode` and `ArtworkMode` move modules,
/// and `tui`'s flags must parse, default and print exactly as before. A
/// bare `tenuto` uses `default()`, so that is pinned too.
#[test]
fn the_tui_flags_keep_their_defaults_and_help() -> Result<(), Box<dyn std::error::Error>> {
    const TUI_HELP: &str = concat!(
        "Open the terminal player on the saved queue\n",
        "\n",
        "Usage: tui [OPTIONS]\n",
        "\n",
        "Options:\n",
        "      --mouse <MOUSE>\n",
        "          Whether the player captures the mouse\n",
        "          \n",
        "          [default: on]\n",
        "          [possible values: on, off]\n",
        "\n",
        "      --artwork <ARTWORK>\n",
        "          How the player draws cover art\n",
        "          \n",
        "          [default: auto]\n",
        "          [possible values: auto, blocks, off]\n",
        "\n",
        "  -h, --help\n",
        "          Print help\n",
    );
    let Some(CliCommand::Tui { mouse, artwork }) = Cli::try_parse_from(["tenuto", "tui"])?.command
    else {
        panic!("`tenuto tui` parses to the tui subcommand");
    };
    assert_eq!((mouse, artwork), (MouseMode::On, ArtworkMode::Auto));
    assert_eq!(
        (MouseMode::default(), ArtworkMode::default()),
        (MouseMode::On, ArtworkMode::Auto)
    );

    let flags = ["tenuto", "tui", "--mouse", "off", "--artwork", "blocks"];
    let Some(CliCommand::Tui { mouse, artwork }) = Cli::try_parse_from(flags)?.command else {
        panic!("the flags parse to the tui subcommand");
    };
    assert_eq!((mouse, artwork), (MouseMode::Off, ArtworkMode::Blocks));

    let mut command = Cli::command();
    let tui = command
        .find_subcommand_mut("tui")
        .expect("tui is a subcommand");
    assert_eq!(tui.render_long_help().to_string(), TUI_HELP);
    Ok(())
}
```

- [ ] **Step 3: Run both suites**

Run: `cargo fmt && cargo test --locked --test cli --test persistence_model`
Expected: `cli` reports 6 passed and `persistence_model` reports 12 passed, 0 failed in both.

- [ ] **Step 4: Run the gate, then commit**

Run the gate from Global Constraints. Expected: all green.

```bash
git add tests/cli.rs tests/persistence_model.rs
git commit -m "test(m9): pin state.json's bytes and tui's flags before the foundation moves"
```

---

### Task 2: Provenance, checkpoint and volume out of `playback`

**Files:**
- Move: `src/playback/provenance.rs` → `src/media/provenance.rs`
- Move: `src/playback/volume.rs` → `src/volume.rs`
- Delete: `src/playback/checkpoint.rs` (its struct goes into `src/resume.rs`)
- Modify: `src/media/mod.rs`, `src/lib.rs`, `src/playback/mod.rs`, `src/resume.rs`, `tests/m9_layering.rs`
- Modify (imports only, by `sed`): every file under `src/` and `tests/` that names `playback::provenance`, `playback::checkpoint`, `playback::volume`, `super::provenance` or `super::volume`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `crate::media::provenance::PositionProvenance`, `crate::resume::PlaybackCheckpoint`, `crate::volume::Volume`, each unchanged in shape. Tasks 3 and 4 import provenance from its new path.

- [ ] **Step 1: RED. Rank `volume` and delete the five entries this task frees**

In `tests/m9_layering.rs`, `LAYERS`, add a line after `("resume", 0),`:

```rust
    ("volume", 0),
```

In `ALLOWED`, delete exactly these five lines:

```rust
    ("src/media/metadata.rs", "playback"),
    ("src/persistence/model.rs", "playback"),
    ("src/persistence/queue_codec.rs", "playback"),
    ("src/playlist/queue.rs", "playback"),
    ("src/resume.rs", "playback"),
```

- [ ] **Step 2: Watch it fail**

Run: `cargo test --locked --test m9_layering the_source_tree`
Expected: FAIL, and among the reported lines exactly these:

```
LAYERS: `volume` is not declared in src/lib.rs
src/media/metadata.rs:5: media (rank 0) -> playback (rank 2)
src/persistence/model.rs:12: persistence (rank 0) -> playback (rank 2)
src/persistence/model.rs:13: persistence (rank 0) -> playback (rank 2)
src/persistence/queue_codec.rs:13: persistence (rank 0) -> playback (rank 2)
src/playlist/queue.rs:9: playlist (rank 0) -> playback (rank 2)
src/resume.rs:20: resume (rank 0) -> playback (rank 2)
```

- [ ] **Step 3: Move the two files and register them**

```bash
git mv src/playback/provenance.rs src/media/provenance.rs
git mv src/playback/volume.rs src/volume.rs
```

- In `src/media/mod.rs`, add `pub mod provenance;` after `pub mod metadata;`.
- In `src/lib.rs`, add `pub mod volume;` after `pub mod tui;`.
- In `src/playback/mod.rs`, delete `pub mod checkpoint;`, `pub mod provenance;` and `pub mod volume;`.
- In `src/media/provenance.rs`'s module doc, `PositionQuality` now lives in another module. Change "orthogonal to `PositionQuality` (`timeline.rs`)" to "orthogonal to `PositionQuality` (`playback::timeline`)".

- [ ] **Step 4: Put `PlaybackCheckpoint` into `resume.rs`**

In `src/resume.rs`, replace the import block

```rust
use std::time::Duration;

use crate::playback::provenance::PositionProvenance;
```

with

```rust
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::media::id::MediaId;
use crate::media::provenance::PositionProvenance;

/// Logical resume position, independent of current transport capabilities.
/// `updated_at` is for inspection, never ordering or merging updates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaybackCheckpoint {
    pub media: MediaId,
    pub position: Duration,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}
```

The struct is `src/playback/checkpoint.rs` verbatim. Then:

```bash
git rm -q src/playback/checkpoint.rs
```

- [ ] **Step 5: Rewrite every import**

```bash
grep -rlE 'playback::(provenance|checkpoint|volume)\b|super::(provenance|volume)::' src tests \
  | xargs sed -i -E \
    -e 's/\b(crate|tenuto)::playback::provenance::/\1::media::provenance::/g' \
    -e 's/\b(crate|tenuto)::playback::checkpoint::/\1::resume::/g' \
    -e 's/\b(crate|tenuto)::playback::volume::/\1::volume::/g' \
    -e 's/\bsuper::provenance::/crate::media::provenance::/g' \
    -e 's/\bsuper::volume::/crate::volume::/g'
cargo fmt
grep -rnE 'playback::(provenance|checkpoint|volume)|super::(provenance|volume)::' src tests
```

Expected: the last `grep` prints nothing. `super::provenance::` and `super::volume::` occur only in `src/playback/{decode,engine,event,wait,command}.rs`, where they now name the new homes.

- [ ] **Step 6: GREEN**

Run: `cargo test --locked --test m9_layering --test persistence_model --test provenance --test resume_contract`
Expected: all pass. `m9_layering` passes with no stale entry, because the seven remaining B entries still excuse `tags`, `vbr_header`, `error`, `display`, `tui/mod` and `tui/images`.

- [ ] **Step 7: Gate, then commit**

Run the gate. Expected: all green. Also run `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`, which catches any intra-doc link the `sed` missed. Expected: no warnings.

```bash
git add -A src tests
git commit -m "refactor(m9): provenance, checkpoint and volume leave playback for the foundation"
```

---

### Task 3: The container probe into `media::probe`, with `ProbeError`

**Files:**
- Create: `src/media/probe.rs`
- Modify: `src/media/mod.rs`, `src/playback/decode.rs`, `src/playback/error.rs`, `src/media/vbr_header.rs`, `src/media/tags.rs`, `src/application/enrich.rs`, `tests/m9_layering.rs`

**Interfaces:**
- Consumes from Task 2: `crate::media::provenance::PositionProvenance`.
- Produces:
  - `pub enum crate::media::probe::ProbeError { Open { path: PathBuf, source: std::io::Error }, UnsupportedInput { path: PathBuf, reason: String }, Decode(symphonia::core::errors::Error), Io(std::io::Error) }`
  - `impl From<ProbeError> for PlaybackError`
  - `pub fn probe_vbr_header(&mut dyn MediaSource) -> Result<Option<VbrHeader>, ProbeError>`
  - `pub fn probe_local_tags(&AbsolutePath) -> Result<LocalTags, ProbeError>`
  - `pub type TagProbe = Arc<dyn Fn(&AbsolutePath) -> Result<LocalTags, ProbeError> + Send + Sync>`
  - `pub(crate)` in `media::probe`: `LocalFile`, `open_local_file`, `ProbedContainer`, `probe_container`, `track_duration`, `StandardNames`, `standard_names`

- [ ] **Step 1: RED. Write the conversion test**

Append to `src/playback/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::path::PathBuf;

    use super::PlaybackError;
    use crate::media::probe::ProbeError;

    fn source_text(error: &dyn Error) -> Option<String> {
        error.source().map(ToString::to_string)
    }

    /// M9 foundation cleanup §3.2: a probe failure converts into the
    /// same-named `PlaybackError` variant with its fields and source moved
    /// across, never wrapped, so its text and chain are what they were.
    #[test]
    fn a_probe_error_becomes_the_same_playback_error() {
        let path = PathBuf::from("/music/a.mp3");

        let probe = ProbeError::Open {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "gone"),
        };
        assert_eq!(probe.to_string(), r#"cannot open media "/music/a.mp3""#);
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), r#"cannot open media "/music/a.mp3""#);
        assert_eq!(source_text(&converted).as_deref(), Some("gone"));
        let PlaybackError::Open { path: got, source } = &converted else {
            panic!("Open expected: {converted:?}");
        };
        assert_eq!(got, &path);
        assert_eq!(source.kind(), io::ErrorKind::NotFound);

        let probe = ProbeError::UnsupportedInput {
            path: path.clone(),
            reason: "not a regular file".into(),
        };
        let text = r#"cannot play "/music/a.mp3": not a regular file"#;
        assert_eq!(probe.to_string(), text);
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), text);
        assert!(source_text(&converted).is_none());
        let PlaybackError::UnsupportedInput { path: got, reason } = &converted else {
            panic!("UnsupportedInput expected: {converted:?}");
        };
        assert_eq!((got, reason.as_str()), (&path, "not a regular file"));

        let probe = ProbeError::Decode(symphonia::core::errors::Error::DecodeError("bad frame"));
        let decode_source = source_text(&probe);
        assert_eq!(probe.to_string(), "cannot decode media");
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), "cannot decode media");
        assert!(matches!(converted, PlaybackError::Decode(_)));
        assert_eq!(source_text(&converted), decode_source);
        assert!(decode_source.is_some());

        let probe = ProbeError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "short"));
        assert_eq!(probe.to_string(), "terminal I/O error");
        let converted = PlaybackError::from(probe);
        assert_eq!(converted.to_string(), "terminal I/O error");
        assert_eq!(source_text(&converted).as_deref(), Some("short"));
        let PlaybackError::Io(source) = &converted else {
            panic!("Io expected: {converted:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::UnexpectedEof);
    }
}
```

The `Io` source check reads `source()`. thiserror's `#[from]` on a tuple variant marks the field as the source, so `"short"` is what it returns, on both sides.

- [ ] **Step 2: RED. Delete the two entries this task frees**

In `tests/m9_layering.rs`, `ALLOWED`, delete:

```rust
    ("src/media/tags.rs", "playback"),
    ("src/media/vbr_header.rs", "playback"),
```

- [ ] **Step 3: Watch both fail**

Run: `cargo test --locked --lib a_probe_error_becomes`
Expected: compile error, `unresolved import `crate::media::probe``.

Run: `cargo test --locked --test m9_layering the_source_tree`
Expected: FAIL, reporting exactly these lines:

```
src/media/tags.rs:13: media (rank 0) -> playback (rank 2)
src/media/tags.rs:17: media (rank 0) -> playback (rank 2)
src/media/vbr_header.rs:19: media (rank 0) -> playback (rank 2)
```

- [ ] **Step 4: Create `src/media/probe.rs`**

Start the file with:

```rust
//! The container probe playback and the local tag reader share (design doc
//! M5 §8): open a regular file, read its MP3 frame-count evidence, and let
//! symphonia recognise the container. No decoder is built and no packet is
//! read.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};
use symphonia::core::units::Timestamp;

use crate::media::id::AbsolutePath;
use crate::media::provenance::PositionProvenance;
use crate::media::vbr_header::{VbrHeader, probe_vbr_header};

/// Why a probe failed. Each variant stands for the same-named
/// `PlaybackError` variant, wording included, and converts into it with
/// its source moved across unchanged.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("cannot open media {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("terminal I/O error")]
    Io(#[from] std::io::Error),
}
```

Then **cut** from `src/playback/decode.rs` everything from the line `/// A regular local file opened for reading, with the extension hint the` (line 395 on `main`) through the closing `}` of `fn year_of` (line 547), and paste it below the enum. In the pasted text only, replace every `PlaybackError` with `ProbeError`. Six occurrences: three `PlaybackError::Open`/`UnsupportedInput` constructors, two return types and one `map_err(PlaybackError::Decode)`.

Then **cut** the `#[cfg(test)] mod year_tests { … }` block (lines 560–571 on `main`) from `decode.rs` and paste it at the end of `probe.rs`, renaming `mod year_tests` to `mod tests`.

In `src/media/mod.rs`, add `pub mod probe;` after `pub mod metadata;`.

- [ ] **Step 5: Point `decode.rs` at the new module**

Replace `decode.rs`'s import block (from `use std::fs::File;` through `use super::error::PlaybackError;`) with:

```rust
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::well_known::FORMAT_ID_MP3;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSource;
use symphonia::core::units::TimeBase;

use crate::media::capabilities::{
    Continuity, DemuxerSeek, MediaCapabilities, SeekSupport, SourceEvidence,
};
use crate::media::id::AbsolutePath;
use crate::media::metadata::MediaMetadata;
use crate::media::probe::{ProbedContainer, open_local_file, probe_container, track_duration};
use crate::media::provenance::PositionProvenance;

use super::error::PlaybackError;
```

`DecodedSource::open` (`open_local_file(path)?`) and `DecodedSource::from_media_source` (`probe_container(source, &hint)?`) now convert through `From<ProbeError>` and need no other edit. If `cargo build --locked` reports an unused import, remove it. If it reports a missing one, add it; the list above was written from a grep, not a compile.

- [ ] **Step 6: The conversion in `src/playback/error.rs`**

Add `use crate::media::probe::ProbeError;` after `use std::path::PathBuf;`, and after the enum:

```rust
/// A probe failure is the playback failure of the same name: fields and
/// source move across unchanged, so text and chain stay what they were.
impl From<ProbeError> for PlaybackError {
    fn from(error: ProbeError) -> Self {
        match error {
            ProbeError::Open { path, source } => Self::Open { path, source },
            ProbeError::UnsupportedInput { path, reason } => {
                Self::UnsupportedInput { path, reason }
            }
            ProbeError::Decode(source) => Self::Decode(source),
            ProbeError::Io(source) => Self::Io(source),
        }
    }
}
```

- [ ] **Step 7: `vbr_header`, `tags` and `enrich` return `ProbeError`**

`src/media/vbr_header.rs`:
- `use crate::playback::error::PlaybackError;` becomes `use crate::media::probe::ProbeError;`.
- `probe_vbr_header`, `gather_evidence` and `read_up_to` change `PlaybackError` to `ProbeError` in their return types.
- In `read_up_to`, `Err(error) => return Err(PlaybackError::from(error)),` becomes `Err(error) => return Err(ProbeError::Io(error)),`.

`src/media/tags.rs`: replace

```rust
use crate::playback::decode::{
    ProbedContainer, StandardNames, open_local_file, probe_container, standard_names,
    track_duration,
};
use crate::playback::error::PlaybackError;
```

with

```rust
use crate::media::probe::{
    ProbeError, ProbedContainer, StandardNames, open_local_file, probe_container,
    standard_names, track_duration,
};
```

Also change `probe_local_tags`'s return type to `Result<LocalTags, ProbeError>`.

`src/application/enrich.rs`: `use crate::playback::error::PlaybackError;` becomes `use crate::media::probe::ProbeError;`, and `TagProbe` becomes:

```rust
pub type TagProbe = Arc<dyn Fn(&AbsolutePath) -> Result<LocalTags, ProbeError> + Send + Sync>;
```

- [ ] **Step 8: GREEN**

Run: `cargo fmt && cargo test --locked --lib a_probe_error_becomes && cargo test --locked --lib year && cargo test --locked --test m9_layering --test m5_metadata --test m5_no_network --test duration_provenance --test engine_contract`
Expected: all pass. `m9_layering` reports no stale entry.

- [ ] **Step 9: Gate, then commit**

Run the gate and rustdoc. Expected: all green, no warnings.

```bash
git add -A src tests
git commit -m "refactor(m9): the container probe moves to media with its own ProbeError"
```

---

### Task 4: `AppError` to `app.rs`; `tui::run` returns `LifecycleError`

**Files:**
- Modify: `src/error.rs`, `src/app.rs`, `src/tui/mod.rs`, `tests/m9_layering.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `crate::app::AppError` (same three transparent arms) and `pub fn crate::tui::run(TuiOptions) -> Result<RunOutcome, LifecycleError>`.

- [ ] **Step 1: RED. Delete the two entries**

In `tests/m9_layering.rs`, `ALLOWED`, delete:

```rust
    ("src/error.rs", "feed"),
    ("src/error.rs", "playback"),
```

- [ ] **Step 2: Watch it fail**

Run: `cargo test --locked --test m9_layering the_source_tree`
Expected: FAIL, reporting exactly:

```
src/error.rs:58: error (rank 0) -> playback (rank 2)
src/error.rs:60: error (rank 0) -> feed (rank 1)
```

- [ ] **Step 3: Move `AppError`**

Cut the `/// What [`crate::app::run`] returns …` doc comment and the `pub enum AppError { … }` block from `src/error.rs`, and paste it into `src/app.rs` after the `use` block. In the pasted doc comment, change ``[`crate::app::run`]`` to ``[`run`]``. In `app.rs`, add `use crate::error::LifecycleError;` to the imports so the pasted `Lifecycle(#[from] LifecycleError)` arm resolves. Replace the two `crate::error::AppError` return types (`run` and `run_resolved`) with `AppError`. Delete the now-redundant `use crate::error::LifecycleError;` inside `run_resolved`.

- [ ] **Step 4: Convert at both `tui::run` call sites in `app::run`**

The bare `tenuto` path:

```rust
        return crate::tui::run(crate::tui::TuiOptions {
            mouse: cli::MouseMode::default(),
            artwork: cli::ArtworkMode::default(),
        })
        .map_err(AppError::from);
```

The `CliCommand::Tui` arm:

```rust
        CliCommand::Tui { mouse, artwork } => {
            crate::tui::run(crate::tui::TuiOptions { mouse, artwork }).map_err(AppError::from)
        }
```

(Task 6 changes `cli::MouseMode` to the `tui` path. Leave it for now.)

- [ ] **Step 5: `tui` returns `LifecycleError`**

In `src/tui/mod.rs`:
- `use crate::error::{AppError, LifecycleError};` becomes `use crate::error::LifecycleError;`.
- `Failed(AppError),` becomes `Failed(LifecycleError),`.
- `pub fn run(options: TuiOptions) -> Result<RunOutcome, AppError>` becomes `-> Result<RunOutcome, LifecycleError>`.
- In `attempt!`, `Err(error) => return Ending::Failed(AppError::from(error)),` becomes `Err(error) => return Ending::Failed(error),`. Every `attempt!` argument already is a `LifecycleError`: `platform_path`/`acquire` map into it, `redirect_to_session_log` returns it on both `cfg` arms, and `enter_terminal` maps to `LifecycleError::Terminal`. So `LifecycleError::from(error)`, which the spec's table names, would be an identity conversion that clippy's `useless_conversion` rejects.
- The five `LifecycleError::Terminal(error).into()` become `LifecycleError::Terminal(error)`.
- `teardown`'s return type becomes `Result<RunOutcome, LifecycleError>`, and `Err(LifecycleError::WorkerPanicked.into())` becomes `Err(LifecycleError::WorkerPanicked)`.

Run: `grep -rn 'AppError' src` and expect matches only in `src/app.rs`.

- [ ] **Step 6: GREEN**

Run: `cargo fmt && cargo test --locked --test m9_layering --test cli --test m5_tui_process --test m5_lock_process`
Expected: all pass (the subprocess suites are Linux-only).

- [ ] **Step 7: Gate, then commit**

Run the gate and rustdoc. Expected: all green, no warnings.

```bash
git add -A src tests
git commit -m "refactor(m9): AppError lives in app.rs, and tui::run returns its LifecycleError"
```

---

### Task 5: `redact_url` to `telemetry`

**Files:**
- Modify: `src/telemetry.rs`, `src/http/error.rs`, `tests/m9_layering.rs`
- Modify (imports only): `src/application/{browse,runtime,source}.rs`, `src/commands.rs`, `src/library.rs`, `src/media/display.rs`, `src/http/{document,service,source}.rs`, `src/playback/{engine,prepare}.rs`, `tests/http_errors.rs`, plus the doc link in `tests/m4_diagnostics.rs`

**Interfaces:**
- Produces: `pub fn crate::telemetry::redact_url(input: &str) -> String`, unchanged.

- [ ] **Step 1: RED. Delete the entry**

In `ALLOWED`, delete `    ("src/media/display.rs", "http"),`.

- [ ] **Step 2: Watch it fail**

Run: `cargo test --locked --test m9_layering the_source_tree`
Expected: FAIL, reporting exactly `src/media/display.rs:12: media (rank 0) -> http (rank 1)`.

- [ ] **Step 3: Move the function**

Cut `redact_url` and its doc comment (`/// Scheme, host, port and path only.` through the closing `}`) from `src/http/error.rs`. Paste it at the end of `src/telemetry.rs`, and add `use url::Url;` to telemetry's imports. In `http/error.rs`:
- delete `use url::Url;`, which is now unused;
- in `RemoteFailure`'s doc comment, change ``[`redact_url`]`` to ``[`crate::telemetry::redact_url`]``.

- [ ] **Step 4: Rewrite the imports**

```bash
perl -0pi -e '
  s{^use (crate::http|super)::error::\{(.*?), redact_url\};}{
    my ($p, $rest) = ($1, $2);
    "use $p\::error::" . ($rest =~ /,/ ? "{$rest}" : $rest) . ";\nuse crate::telemetry::redact_url;"
  }gme;
  s{^use crate::http::error::redact_url;}{use crate::telemetry::redact_url;}gm;
' src/application/browse.rs src/application/runtime.rs src/application/source.rs \
  src/commands.rs src/library.rs src/media/display.rs \
  src/http/document.rs src/http/service.rs src/http/source.rs \
  src/playback/engine.rs src/playback/prepare.rs
perl -0pi -e 's{^use tenuto::http::error::\{(.*?), redact_url\};}{use tenuto::http::error::{$1};\nuse tenuto::telemetry::redact_url;}m' tests/http_errors.rs
sed -i 's/`tenuto::http::error::redact_url`/`tenuto::telemetry::redact_url`/' tests/m4_diagnostics.rs
cargo fmt
grep -rn 'error::redact_url\|, redact_url}' src tests
```

Expected: the final `grep` prints nothing.

- [ ] **Step 5: GREEN**

Run: `cargo test --locked --test m9_layering --test m4_diagnostics --test http_errors`
Expected: all pass.

- [ ] **Step 6: Gate, then commit**

Run the gate and rustdoc. Expected: all green, no warnings.

```bash
git add -A src tests
git commit -m "refactor(m9): redact_url moves to telemetry, below every module that reports a URL"
```

---

### Task 6: `MouseMode` and `ArtworkMode` to `tui`

**Files:**
- Modify: `src/cli.rs`, `src/tui/mod.rs`, `src/tui/images.rs`, `src/app.rs`, `tests/m5_tui_images.rs`, `tests/cli.rs`, `tests/m9_layering.rs`

**Interfaces:**
- Produces: `crate::tui::MouseMode` and `crate::tui::images::ArtworkMode`, unchanged (derives `Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum`).

- [ ] **Step 1: RED. Delete the last two B entries and the B header**

In `ALLOWED`, delete:

```rust
    // B: foundation cleanup.
    ("src/tui/mod.rs", "cli"),
    ("src/tui/images.rs", "cli"),
```

`ALLOWED` now holds only the `// C` comment and its six entries.

- [ ] **Step 2: Watch it fail**

Run: `cargo test --locked --test m9_layering the_source_tree`
Expected: FAIL, reporting exactly:

```
src/tui/images.rs:26: tui (rank 4) -> cli (rank 5)
src/tui/mod.rs:60: tui (rank 4) -> cli (rank 5)
```

- [ ] **Step 3: Move the enums**

- Cut `MouseMode` with its doc comment and derive from `src/cli.rs`, and paste it into `src/tui/mod.rs` directly above `pub struct TuiOptions`.
- Cut `ArtworkMode` the same way and paste it into `src/tui/images.rs` after its `use` block.
- In `src/tui/images.rs`, delete `use crate::cli::ArtworkMode;`.
- In `src/tui/mod.rs`, `use crate::cli::{ArtworkMode, MouseMode};` becomes `use crate::tui::images::ArtworkMode;`.
- In `src/cli.rs`, add:

```rust
use crate::tui::MouseMode;
use crate::tui::images::ArtworkMode;
```

- In `src/app.rs`, `cli::MouseMode::default()` becomes `crate::tui::MouseMode::default()`, and `cli::ArtworkMode::default()` becomes `crate::tui::images::ArtworkMode::default()`.
- In `tests/m5_tui_images.rs`, `use tenuto::cli::ArtworkMode;` becomes `use tenuto::tui::images::ArtworkMode;`.
- In `tests/cli.rs`, `use tenuto::cli::{ArtworkMode, Cli, CliCommand, MouseMode};` becomes:

```rust
use tenuto::cli::{Cli, CliCommand};
use tenuto::tui::MouseMode;
use tenuto::tui::images::ArtworkMode;
```

Then run: `grep -rn 'cli::ArtworkMode\|cli::MouseMode\|cli::{ArtworkMode' src tests` and expect nothing.

- [ ] **Step 4: GREEN**

Run: `cargo fmt && cargo test --locked --test m9_layering --test cli --test m5_tui_images`
Expected: all pass. `the_tui_flags_keep_their_defaults_and_help` passes with its literal unchanged.

- [ ] **Step 5: Gate, then commit**

Run the gate and rustdoc. Expected: all green, no warnings.

```bash
git add -A src tests
git commit -m "refactor(m9): the tui's option enums live in tui, and cli imports them"
```

---

### Task 7: Documentation

**Files:**
- Modify: `docs/architecture.md` (§4 diagram lines 142 and 146, component table lines 190/195/203, the "Foundation reaches up" bullet at line 211, and §10 line 497)

- [ ] **Step 1: Edit `docs/architecture.md`**

- Line 142: `media["media/<br/>MediaId, capabilities,<br/>metadata, tags, VBR header"]` becomes `media["media/<br/>MediaId, capabilities, provenance,<br/>metadata, container probe, tags, VBR header"]`.
- Line 146: `clock["clock.rs, telemetry.rs, error.rs"]` becomes `clock["clock.rs, telemetry.rs, error.rs, volume.rs"]`.
- Table, `app` row, responsibility cell: append ` Own `AppError`, the transparent union `run` returns.`
- Table, `playlist`/`resume` row: `` `resume` is the resume decision from a position and a completion flag. `` becomes `` `resume` is the resume decision from a position and a completion flag, and holds `PlaybackCheckpoint`, the logical resume position. ``
- Table, `media` row: `Validating identities, capabilities, metadata, tag and VBR-header reading.` becomes `Validating identities, capabilities, position provenance, metadata, the container probe (`media::probe`, shared with playback), tag and VBR-header reading.`
- Delete the whole bullet that starts `- Foundation reaches up.`.
- Line 497: `` `AppError` is the transparent union `app::run` returns. `` becomes `` `AppError` (`app.rs`) is the transparent union `app::run` returns; `tui::run` returns the `LifecycleError` it can only fail with, and `app::run` converts it. ``

- [ ] **Step 2: Verify**

Run: `grep -n 'Foundation reaches up\|playback::decode\|cli::ArtworkMode' docs/architecture.md`
Expected: nothing.

Run the full gate plus rustdoc one last time, and `bash scripts/release/test.sh`. Expected: all green.

- [ ] **Step 3: Commit**

```bash
git add docs/architecture.md
git commit -m "docs(m9): architecture names the foundation's new homes and drops the B exceptions"
```
