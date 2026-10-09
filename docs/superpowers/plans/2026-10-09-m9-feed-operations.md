# M9 Feed Operations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete the 6 `// C` entries from `tests/m9_layering.rs`'s `ALLOWED`, leaving it empty, by moving feed and station mutations into one `application::feed_ops` dispatch, `displayable` into `telemetry`, and the platform store constructors onto `LibraryStores`. CLI output, exit codes, errors, logs and TUI notices do not change.

**Architecture:** Four code tasks and one documentation/verification task. Tasks 1–3 each start by deleting their own `ALLOWED` entries, so the layering test names the edges (RED), then move code until it passes (GREEN). Task 3 introduces `feed_ops` with a temporary conversion in `browse.rs`. Task 4 replaces `BrowseRequest`'s mutation variants with `Op(FeedOp)`, which removes that conversion. Task 5 updates the docs, runs the gate and the manual parity comparison, and writes the acceptance record.

**Tech Stack:** Rust 1.98.1, edition 2024, thiserror 2, tokio (behind `HttpService`), `directories`, `tempfile` (dev).

**Spec:** `docs/superpowers/specs/2026-10-09-tenuto-m9-feed-operations-design.md`. Read it before starting; §2 (compatibility boundary) is the contract every task keeps.

## Global Constraints

- Every cargo command uses `--locked`; toolchain 1.98.1.
- Runtime code has no `unsafe`, `unwrap` or `expect` (the lints deny them). Tests may use them (`clippy.toml`).
- Compatibility (spec §2): every CLI stdout line, the stderr line, exit codes, each command's failure order, the `FeedError` value inside `AppError::Feed` (so the `Display` line and the `?error` Debug log), TUI notice text and newline layout, `describe` log strings, and "local operations never spawn an `HttpService`" stay as they are.
- `FeedOp` is `pub`. `Report`, `run`, `wait_http` and `Report::into_notice` are `pub(crate)`. No `pub use` shims at old paths.
- Never add to `ALLOWED`. Each task deletes exactly the entries it names.
- Moved items keep their doc comments; update only the paths and names inside them.
- Commits carry no `Co-authored-by` or tool attribution.
- Work on branch `refactor/m9-feed-operations`, which already holds the spec commit `fd11069`.
- The gate at each task's end is `cargo fmt --check`, then `cargo clippy --locked --all-targets --all-features -- -D warnings`, then `cargo test --locked --no-fail-fast`.

## Review Focus

1. **A TUI refresh-all where one feed fails.** The notice must be red and read every per-feed line, then a newline, then the `BatchIncomplete` message, exactly as today. *Pinned in Task 3:* `a_mixed_batch_becomes_one_failed_notice`.
2. **Removing a feed or a station while offline.** It must not spawn a Tokio runtime or touch the network. *Pinned in Task 3:* `local_operations_leave_the_http_slot_empty`.
3. **`tenuto refresh … | head -0` or a closed stdout.** A stdout failure must outrank the operation's own error; an operation that fails before printing must still report its own error. *Pinned in Task 3:* `an_unwritable_stream_fails_the_command` (the stdout half, with a failing report), and in Task 5's parity run (scenario 2b, stdout closed).
4. **A feed or station URL carrying credentials or a token.** The browse worker's log line must still redact it after the variants change shape. *Pinned in Task 4:* `describe_redacts_feed_and_station_urls`.
5. **`tenuto subscribe <url> --as <slug>`.** The CLI's own slug must still reach `library::subscribe` now that the TUI's `Subscribe` carries `slug: None`. *Pinned by the existing suite:* `tests/m4_cli.rs` subscribes with `--as radio-t` in a subprocess and then lists and refreshes under that slug; Task 3 Step 9 runs it by name. Task 5's parity run (scenario 1) uses `--as` too.

---

### Task 1: `displayable` to `telemetry`

**Files:**
- Modify: `tests/m9_layering.rs:69-77` (delete 4 entries)
- Modify: `src/telemetry.rs` (append `displayable`, `needs_escape`)
- Modify: `src/commands.rs` (delete `displayable`, `needs_escape`; import from `telemetry`)
- Modify: `src/application/runtime.rs:33`, `src/application/view.rs:10`, `src/lifecycle/panic.rs:26,253`, `src/tui/render/browser.rs:14`, `src/app.rs:364`
- Modify: `tests/m4_diagnostics.rs:30,634`, `tests/m8_tui.rs:296` (doc comments only)

**Interfaces:**
- Consumes: nothing new.
- Produces: `crate::telemetry::displayable(text: &str) -> String`, `pub(crate)`. Task 3's `feed_ops` does not use it; `commands::title_text` does.

- [ ] **Step 1: Delete the four `displayable` entries from `ALLOWED`**

In `tests/m9_layering.rs`, delete these four lines and keep the rest of the list:

```rust
    ("src/application/runtime.rs", "commands"),
    ("src/application/view.rs", "commands"),
    ("src/lifecycle/panic.rs", "commands"),
    ("src/tui/render/browser.rs", "commands"),
```

- [ ] **Step 2: Run the layering test to verify it fails**

Run: `cargo test --locked --test m9_layering`
Expected: FAIL, naming exactly these four edges (line numbers as of `5582b5c`):

```
src/application/runtime.rs:33: application (rank 3) -> commands (rank 5)
src/application/view.rs:10: application (rank 3) -> commands (rank 5)
src/lifecycle/panic.rs:26: lifecycle (rank 0) -> commands (rank 5)
src/tui/render/browser.rs:14: tui (rank 4) -> commands (rank 5)
```

- [ ] **Step 3: Move `displayable` and `needs_escape` to `src/telemetry.rs`**

Cut `pub(crate) fn displayable` and `fn needs_escape` from `src/commands.rs`, with their doc comments, and append them to `src/telemetry.rs` unchanged.

Note: in `commands.rs` today, the doc comment above `reversed` ("The listing in reverse, cut to `limit` from the end…") is preceded by a paragraph that belongs to `displayable` ("Feed-supplied text reaching a terminal, made safe **at the formatting boundary only**…"). The two comments run together. Move the `displayable` paragraph with `displayable`, so it becomes its doc comment in `telemetry.rs`, and leave `reversed` with only its own comment. The result in `telemetry.rs`:

```rust
/// Feed-supplied text reaching a terminal, made safe **at the formatting
/// boundary only**: the cached title and the identity derived from it are
/// untouched, so nothing here changes what a later refresh compares against.
/// A newline would break the row layout and an escape sequence would reach
/// the terminal, so every character that can do either becomes a visible
/// escape; all the rest — Cyrillic, CJK, emoji — pass through exactly as
/// stored, since transliterating a title would make it someone else's title.
pub(crate) fn displayable(text: &str) -> String {
    // body unchanged
}

/// What a title may not carry into a row.
/// (the rest of the existing doc comment, unchanged)
fn needs_escape(value: char) -> bool {
    // body unchanged
}
```

- [ ] **Step 4: Rewrite the callers**

- `src/commands.rs`: add `use crate::telemetry::displayable;`. Merge it with the existing `use crate::telemetry::redact_url;` as `use crate::telemetry::{displayable, redact_url};`.
- `src/application/runtime.rs:33`, `src/application/view.rs:10`, `src/lifecycle/panic.rs:26`, `src/tui/render/browser.rs:14`: `use crate::commands::displayable;` becomes `use crate::telemetry::displayable;`. Keep the `use` blocks sorted the way `cargo fmt` leaves them.
- `src/app.rs:364`: `crate::commands::displayable(` becomes `crate::telemetry::displayable(`.
- Doc comments: `src/lifecycle/panic.rs:253`, `tests/m4_diagnostics.rs:30` and `:634`, and `tests/m8_tui.rs:296`. Each spelling of `commands::displayable` becomes `telemetry::displayable`. In `m8_tui.rs:296` the text says "through `displayable`"; leave it as it is if it doesn't name `commands`.

- [ ] **Step 5: Run the layering test and the escaping tests**

Run: `cargo test --locked --test m9_layering && cargo test --locked --lib commands::tests`
Expected: PASS. `m9_layering` no longer reports those four edges; the two remaining `// C` entries still excuse real edges. The `commands` tests include `titles_are_defanged_without_being_rewritten` and `line_separators_and_bidi_overrides_are_escaped_too`, which exercise `displayable` through the listing tables.

- [ ] **Step 6: Gate**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add -A src tests
git commit -m "refactor(m9): displayable moves to telemetry, beside redact_url"
```

---

### Task 2: `LibraryStores::platform`

**Files:**
- Modify: `tests/m9_layering.rs` (delete 1 entry)
- Modify: `src/application/runtime.rs` (add `impl LibraryStores { pub fn platform() }`)
- Modify: `src/commands.rs` (delete `platform_subscription_stores`, `platform_station_store`; `Feeds`, `Episodes`, `Subscribe`, `Unsubscribe`, `Refresh` arms)
- Modify: `src/app.rs:103`
- Modify: `src/tui/mod.rs:281,307-318,649`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub fn LibraryStores::platform() -> Result<LibraryStores, FeedError>` in `crate::application::runtime`. Tasks 3 and 4 call it from `commands`.

- [ ] **Step 1: Delete the `tui/mod.rs` entry from `ALLOWED`**

```rust
    ("src/tui/mod.rs", "commands"),
```

- [ ] **Step 2: Run the layering test to verify it fails**

Run: `cargo test --locked --test m9_layering`
Expected: FAIL with

```
src/tui/mod.rs:311: tui (rank 4) -> commands (rank 5)
src/tui/mod.rs:312: tui (rank 4) -> commands (rank 5)
```

- [ ] **Step 3: Add `LibraryStores::platform` beside the struct in `src/application/runtime.rs`**

Directly after `pub struct LibraryStores { … }`:

```rust
impl LibraryStores {
    /// The stores on this platform's directories: `subscriptions.json` and
    /// `stations.json` in the data directory, the feed cache under the cache
    /// directory's `feeds`.
    ///
    /// No constructor touches the filesystem: each file comes into existence
    /// on its first write, so a listing on a machine that has never
    /// subscribed creates nothing (M4 §8.4), and a command that never uses
    /// the station store pays nothing for it. The one failure, no platform
    /// data directory, is reported as the subscriptions being unreadable,
    /// which is what every feed command has always said.
    pub fn platform() -> Result<Self, FeedError> {
        let dirs = directories::ProjectDirs::from("", "", "tenuto").ok_or_else(|| {
            FeedError::SubscriptionsUnreadable {
                reason: "no platform data directory is available".to_string(),
            }
        })?;
        let clock = Arc::new(SystemClock);
        Ok(Self {
            subscriptions: SubscriptionStore::new(
                dirs.data_dir().join("subscriptions.json"),
                Arc::clone(&clock),
            ),
            cache: CacheStore::new(dirs.cache_dir().join("feeds")),
            stations: StationStore::new(dirs.data_dir().join("stations.json"), clock),
        })
    }
}
```

Add the imports `runtime.rs` lacks: `crate::clock::SystemClock` (beside the existing `crate::clock::Clock`) and `crate::feed::error::FeedError` (if not already imported). If `Arc::clone(&clock)` doesn't coerce to the store's `Arc<dyn Clock>` parameter, write `let clock: Arc<dyn Clock> = Arc::new(SystemClock);`.

- [ ] **Step 4: Delete the two constructors from `src/commands.rs` and rewrite their callers**

Delete `platform_subscription_stores` and `platform_station_store` with their doc comments. Keep `platform_state_store`. Its doc comment mentions [`platform_subscription_stores`] only indirectly; if it links to it, reword the link to `LibraryStores::platform`.

Add `use crate::application::runtime::LibraryStores;` to `commands.rs`, and rewrite each arm in `run`:

```rust
        CliCommand::Feeds => {
            let stores = LibraryStores::platform()?;
            write_feeds(
                &mut out,
                &crate::library::list_feeds(&stores.subscriptions, &stores.cache)?,
            )
        }
```

In `Episodes`, `let (subs, cache) = platform_subscription_stores()?;` becomes:

```rust
            let LibraryStores {
                subscriptions: subs,
                cache,
                ..
            } = LibraryStores::platform()?;
```

Use the same destructuring in `Subscribe`, `Unsubscribe` and both `Refresh` arms (Task 3 replaces those arms). Every other line in those arms stays as it is.

Remove imports that are now unused: `crate::clock::SystemClock`, `crate::station::store::StationStore`, `crate::subscription::store::SubscriptionStore`, `crate::feed::cache::CacheStore`, and `std::sync::Arc` if nothing else uses it. Clippy names any that remain.

- [ ] **Step 5: Rewrite `src/app.rs:103`**

```rust
            let crate::application::runtime::LibraryStores {
                subscriptions: subs,
                cache,
                ..
            } = crate::application::runtime::LibraryStores::platform()?;
```

The next line, `crate::library::resolve_episode(&subs, &cache, …)`, is unchanged.

- [ ] **Step 6: Replace `library_stores()` in `src/tui/mod.rs`**

Delete `fn library_stores()` and its doc comment (lines 307–318). At its two call sites, line 281 (`library: library_stores(),`) and line 649 (`BrowseWorker::spawn(library_stores())`), write `LibraryStores::platform().ok()`. `LibraryStores` is already imported in `tui/mod.rs`; keep that import.

- [ ] **Step 7: Run the layering test and the store-path suites**

Run: `cargo test --locked --test m9_layering && cargo test --locked --test m6_feed_management --test m5_no_network`
Expected: PASS. The subprocess suites run with their own `XDG_DATA_HOME` and `XDG_CACHE_HOME` (`tests/support/process.rs`), so they confirm that `subscriptions.json`, `stations.json` and the cache still land at today's paths.

- [ ] **Step 8: Gate**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add -A src tests
git commit -m "refactor(m9): the platform stores come from LibraryStores::platform"
```

---

### Task 3: `application::feed_ops`, the one mutation dispatch

**Files:**
- Modify: `tests/m9_layering.rs` (delete the last entry and the `// C` comment; `ALLOWED` becomes empty)
- Create: `src/application/feed_ops.rs`
- Modify: `src/application/mod.rs` (declare `pub mod feed_ops;`)
- Modify: `src/commands.rs` (delete `wait_http`, `report`, the seven `finish_*`, `write_refresh`, `write_station_probe`, `station_identity_text`, `ABSENT`, and their tests; add `operate`, `write_report`)
- Modify: `src/application/browse.rs` (imports; `mutate`; delete `http_service`)
- Modify: `tests/m4_diagnostics.rs:649` (doc table path)

**Interfaces:**
- Consumes: `LibraryStores` (Task 2), the `library` mutation functions (unchanged).
- Produces (Task 4 relies on these names):
  - `pub enum FeedOp { Subscribe { url: String, slug: Option<String> }, Unsubscribe { slug: String }, Refresh { slug: Option<String> }, AddStation { url: String }, RemoveStation { slug: String }, ReprobeStation { slug: String } }` with `#[derive(Clone, Debug, Eq, PartialEq)]`;
  - `pub(crate) struct Report { pub text: String, pub outcome: Result<(), FeedError> }`;
  - `pub(crate) fn run(op: &FeedOp, stores: &LibraryStores, http: &mut Option<Arc<HttpService>>) -> Report`;
  - `pub(crate) fn Report::into_notice(self) -> Result<String, String>`;
  - `pub(crate) fn wait_http`, `pub(crate) const ABSENT`.

- [ ] **Step 1: Empty `ALLOWED`**

In `tests/m9_layering.rs`, `ALLOWED` becomes:

```rust
/// `(file, target module)` pairs excused today. Delete an entry with the
/// edge it excuses.
const ALLOWED: &[(&str, &str)] = &[];
```

- [ ] **Step 2: Run the layering test to verify it fails**

Run: `cargo test --locked --test m9_layering`
Expected: FAIL with `src/application/browse.rs:28: application (rank 3) -> commands (rank 5)`.

- [ ] **Step 3: Create `src/application/feed_ops.rs` with its tests**

Declare it in `src/application/mod.rs`, in alphabetical order (`pub mod feed_ops;` after `pub mod enrich;`).

The formatters' bodies are today's `commands.rs` code. Each `writeln!(out, …).map_err(stdout_failure)?` becomes `let _ = writeln!(text, …);` into a `String`. Doc comments carry over; edit only where they name `report()`, `out` or `commands`. The file:

```rust
//! Feed and station changes (M9 PR C): the one dispatch behind `tenuto
//! subscribe`, `unsubscribe` and `refresh` and the browser's feed and
//! station keys. Each operation returns a [`Report`]: the text the CLI
//! prints on stdout, and whether the operation completed. A partial success
//! never counts as complete (M4 design doc §6.4). The CLI prints the text
//! and maps the outcome to its exit status (`commands`); the browser shows
//! it as a notice ([`Report::into_notice`]). Listing columns stay in
//! `commands`.
//!
//! [`wait_http`] is the one place the CLI and the browse worker enter the
//! Tokio runtime, which keeps `block_on` out of `library`.
//!
//! Report text is built in a `String`, which cannot fail to grow, so each
//! `writeln!` result is discarded.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;

use crate::application::runtime::LibraryStores;
use crate::feed::error::FeedError;
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::StationIdentity;
use crate::library::{
    AddStationOutcome, FollowupStep, RefreshOutcome, RemoveStationOutcome, SubscribeOutcome,
    UnsubscribeOutcome, add_station, refresh, refresh_all, remove_station, reprobe_station,
    subscribe, unsubscribe,
};
use crate::telemetry::redact_url;

/// The em dash every absent value prints: no episode count, no publication
/// date, no checkpoint, no station identity. One spelling, so a reader never
/// has to decide whether two blanks mean the same thing.
pub(crate) const ABSENT: &str = "—";

/// A change to the feed library or the saved stations, from the CLI or the
/// browser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedOp {
    /// `tenuto subscribe <url> [--as slug]`; the browser derives the slug.
    Subscribe { url: String, slug: Option<String> },
    /// `tenuto unsubscribe <slug>`. Local-only.
    Unsubscribe { slug: String },
    /// `tenuto refresh [slug]`: one feed, or every feed for `None`.
    Refresh { slug: Option<String> },
    /// Validates, probes and saves a station by URL (M7.1 §6, §10).
    AddStation { url: String },
    /// Drops a saved station. Local-only, like `Unsubscribe` (M7.1 §6).
    RemoveStation { slug: String },
    /// Re-probes a saved station, refreshing its cached identity (M7.1 §6).
    ReprobeStation { slug: String },
}

/// What one [`FeedOp`] did.
#[derive(Debug)]
pub(crate) struct Report {
    /// Exactly what the CLI prints on stdout, trailing newline included.
    /// Empty when the operation failed before it had anything to report.
    pub text: String,
    /// `Err` when the operation is incomplete: the error the CLI's exit
    /// status reports, after `text` has been printed.
    pub outcome: Result<(), FeedError>,
}

impl Report {
    /// The report as the browser's notice: `Ok` is what stdout would have
    /// carried, `Err` that text (when any) followed by the error the exit
    /// status would have named. Trailing whitespace is dropped; the TUI
    /// splits on the rest.
    pub(crate) fn into_notice(self) -> Result<String, String> {
        let text = self.text.trim_end().to_owned();
        match self.outcome {
            Ok(()) => Ok(text),
            Err(error) if text.is_empty() => Err(error.to_string()),
            Err(error) => Err(format!("{text}\n{error}")),
        }
    }
}

/// Runs `op` against `stores`. An operation that needs the network takes
/// the service from `http`, spawning one into it first if it is empty;
/// `Unsubscribe` and `RemoveStation` never touch it (M7.1 §6, R3).
pub(crate) fn run(
    op: &FeedOp,
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
) -> Report {
    dispatch(op, stores, http).unwrap_or_else(|error| Report {
        text: String::new(),
        outcome: Err(error),
    })
}

fn dispatch(
    op: &FeedOp,
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
) -> Result<Report, FeedError> {
    let subs = &stores.subscriptions;
    let cache = &stores.cache;
    let stations = &stores.stations;
    Ok(match op {
        FeedOp::Subscribe { url, slug } => {
            let service = service(http)?;
            finish_subscribe(wait_http(
                &service,
                subscribe(&service, subs, cache, url, slug.as_deref()),
            )?)
        }
        FeedOp::Unsubscribe { slug } => finish_unsubscribe(unsubscribe(subs, cache, slug)?),
        FeedOp::Refresh { slug: Some(slug) } => {
            let service = service(http)?;
            finish_refresh_one(wait_http(&service, refresh(&service, subs, cache, slug))?)
        }
        FeedOp::Refresh { slug: None } => {
            let service = service(http)?;
            finish_refresh_batch(wait_http(&service, refresh_all(&service, subs, cache))?)
        }
        FeedOp::AddStation { url } => {
            let service = service(http)?;
            finish_add_station(wait_http(&service, add_station(&service, stations, url))?)
        }
        FeedOp::RemoveStation { slug } => finish_remove_station(remove_station(stations, slug)?),
        FeedOp::ReprobeStation { slug } => {
            let service = service(http)?;
            finish_reprobe_station(wait_http(
                &service,
                reprobe_station(&service, stations, slug),
            )?)
        }
    })
}

/// The caller's HTTP service, spawned into `slot` on first use. A failure
/// to start is this operation's error; the slot stays empty, so the next
/// operation tries again.
fn service(slot: &mut Option<Arc<HttpService>>) -> Result<Arc<HttpService>, FeedError> {
    if let Some(service) = slot {
        return Ok(Arc::clone(service));
    }
    let service = HttpService::spawn(Limits::default())?;
    *slot = Some(Arc::clone(&service));
    Ok(service)
}

/// The shared synchronous bridge (M4 design doc §6.6). Every feed operation
/// enters the runtime here and nowhere else: `library.rs` stays free of
/// `block_on`, and `run_resolved`'s decoder path never enters a runtime at
/// all.
pub(crate) fn wait_http<F: Future>(service: &HttpService, future: F) -> F::Output {
    service.handle().block_on(future)
}
```

Then the formatters. Each is today's function, converted as described. `write_refresh` (its existing doc comment carries over unchanged):

```rust
fn write_refresh(text: &mut String, outcome: &RefreshOutcome) {
    let (slug, url_moved, followup) = match outcome {
        RefreshOutcome::Updated {
            slug,
            retained,
            skipped,
            url_moved,
            followup,
        } => {
            let _ = writeln!(
                text,
                "{slug}: updated, {retained} episodes retained, {skipped} skipped"
            );
            (slug, url_moved, followup)
        }
        RefreshOutcome::Unchanged {
            slug,
            url_moved,
            followup,
        } => {
            let _ = writeln!(text, "{slug}: unchanged");
            (slug, url_moved, followup)
        }
        RefreshOutcome::Failed { slug, error } => {
            // Nothing committed, so there is no redirect to report and no
            // followup to name: the fetch, the parse or the cache write is
            // the whole story.
            let _ = writeln!(text, "{slug}: failed: {error}");
            return;
        }
    };

    if let Some(moved) = url_moved {
        let _ = writeln!(text, "{slug}: feed URL moved to {}", redact_url(moved));
    }
    if let Some(failure) = followup {
        // The cache is the first half of every refresh commit (§5.3), so it
        // is what did land whichever step failed after it.
        let detail = match (failure.step, url_moved.is_some()) {
            (FollowupStep::SaveSubscription, true) => {
                "the redirected feed URL could not be recorded"
            }
            (FollowupStep::SaveSubscription, false) => {
                "the feed's changed title could not be recorded"
            }
            (FollowupStep::RemoveCache, _) => "a stale cache entry could not be removed",
        };
        let _ = writeln!(
            text,
            "{slug}: the episode cache was saved, but {detail}: {}",
            failure.error
        );
    }
}

/// `tenuto refresh` with no slug (§6.4): every feed is reported, and only
/// then does the count of feeds that did not complete decide the outcome.
/// One bad feed neither hides the others nor completes.
fn finish_refresh_batch(outcomes: Vec<RefreshOutcome>) -> Report {
    let total = outcomes.len();
    let mut text = String::new();
    let mut failed = 0;
    for outcome in &outcomes {
        let bad = match outcome {
            RefreshOutcome::Failed { .. } => true,
            RefreshOutcome::Updated { followup, .. }
            | RefreshOutcome::Unchanged { followup, .. } => followup.is_some(),
        };
        write_refresh(&mut text, outcome);
        failed += usize::from(bad);
    }
    let outcome = if failed == 0 {
        Ok(())
    } else {
        Err(FeedError::BatchIncomplete { failed, total })
    };
    Report { text, outcome }
}

/// `tenuto refresh <slug>` (§6.4): the concrete error, after reporting what
/// did commit. A batch count would tell a single-feed caller nothing it did
/// not already know.
fn finish_refresh_one(outcome: RefreshOutcome) -> Report {
    let mut text = String::new();
    write_refresh(&mut text, &outcome);
    let outcome = match outcome {
        RefreshOutcome::Failed { error, .. } => Err(error),
        RefreshOutcome::Updated { followup, .. } | RefreshOutcome::Unchanged { followup, .. } => {
            followup.map_or(Ok(()), |failure| Err(failure.error))
        }
    };
    Report { text, outcome }
}

/// (today's `finish_subscribe` doc comment, unchanged)
fn finish_subscribe(outcome: SubscribeOutcome) -> Report {
    let SubscribeOutcome {
        slug,
        retained,
        skipped,
        followup,
        ..
    } = outcome;
    let mut text = String::new();
    let Some(failure) = followup else {
        let _ = writeln!(
            text,
            "{slug}: subscribed, {retained} episodes retained, {skipped} skipped"
        );
        return Report {
            text,
            outcome: Ok(()),
        };
    };
    let _ = writeln!(
        text,
        "{slug}: {retained} episodes were cached, but the subscription itself \
         could not be saved; nothing is subscribed: {}",
        failure.error
    );
    Report {
        text,
        outcome: Err(failure.error),
    }
}

/// (today's `finish_unsubscribe` doc comment, unchanged)
fn finish_unsubscribe(outcome: UnsubscribeOutcome) -> Report {
    let UnsubscribeOutcome { slug, followup } = outcome;
    let mut text = String::new();
    let Some(failure) = followup else {
        let _ = writeln!(text, "{slug}: unsubscribed");
        return Report {
            text,
            outcome: Ok(()),
        };
    };
    let _ = writeln!(
        text,
        "{slug}: the subscription was removed, but its cached episodes \
         could not be deleted: {}",
        failure.error
    );
    Report {
        text,
        outcome: Err(failure.error),
    }
}

/// (today's `station_identity_text` doc comment, unchanged)
fn station_identity_text(identity: &StationIdentity) -> String {
    // body unchanged
}

/// `AddStation`'s and `ReprobeStation`'s shared report (M7.1 design doc §10),
/// `action` naming which one so the same taxonomy reads as "added" or
/// "re-probed" rather than composing two near-identical formatters.
fn station_probe(outcome: AddStationOutcome, action: &str) -> Report {
    let mut text = String::new();
    let outcome = match outcome {
        AddStationOutcome::Verified { slug, identity } => {
            let _ = writeln!(
                text,
                "{slug}: {action}, verified — {}",
                station_identity_text(&identity)
            );
            Ok(())
        }
        AddStationOutcome::Unverified { slug, reason } => {
            let _ = writeln!(text, "{slug}: {action}, unverified: {reason}");
            Ok(())
        }
        // A duplicate add resolves to the station already saved (§10) and
        // is not itself an error.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: None,
        } => {
            let _ = writeln!(
                text,
                "{slug}: already saved, re-probed — {}",
                identity
                    .as_ref()
                    .map_or_else(|| ABSENT.to_string(), station_identity_text)
            );
            Ok(())
        }
        // A failed implicit re-probe is reported in the same line rather
        // than swallowed, exactly as an explicit `ReprobeStation`'s failure
        // is (`AddStationOutcome::AlreadySaved`'s own doc comment explains
        // why this field exists at all) — and, like `finish_subscribe` and
        // `finish_unsubscribe`'s own partial-failure arms, the text is
        // reported and the outcome is still `Err`. `Ok` here would undo that
        // fix at one remove: the reason would sit in the string, but
        // `into_notice` would call it a success, the tracing line would drop
        // its `failed:` prefix, and the browser would paint it with
        // `NoticeKind::Ok` instead of the error colour.
        AddStationOutcome::AlreadySaved {
            slug,
            identity,
            reprobe_failure: Some(reason),
        } => {
            let _ = writeln!(
                text,
                "{slug}: already saved; re-probe failed: {reason} (last known: {})",
                identity
                    .as_ref()
                    .map_or_else(|| ABSENT.to_string(), station_identity_text)
            );
            Err(FeedError::StationsUnreadable { reason })
        }
    };
    Report { text, outcome }
}

/// `AddStation` (M7.1 design doc §6, §10).
fn finish_add_station(outcome: AddStationOutcome) -> Report {
    station_probe(outcome, "added")
}

/// (today's `finish_reprobe_station` doc comment, unchanged)
fn finish_reprobe_station(outcome: AddStationOutcome) -> Report {
    station_probe(outcome, "re-probed")
}

/// `RemoveStation` (M7.1 design doc §6): a local edit, always complete once
/// [`crate::library::remove_station`] returns `Ok`.
fn finish_remove_station(outcome: RemoveStationOutcome) -> Report {
    let RemoveStationOutcome { slug } = outcome;
    Report {
        text: format!("{slug}: removed\n"),
        outcome: Ok(()),
    }
}
```

Then the tests, appended to the same file. The first twelve are moved from `commands.rs`'s `mod tests`, with the same names, inputs and assertions. Only the call shape changes: `finish_x(&mut out, outcome)?` becomes `let report = finish_x(outcome); report.outcome?;` followed by assertions on `report.text`, and `.err().ok_or(…)` reads `report.outcome`. The last three are new.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use crate::clock::SystemClock;
    use crate::feed::cache::CacheStore;
    use crate::library::FollowupFailure;
    use crate::station::store::StationStore;
    use crate::subscription::store::SubscriptionStore;

    type Fallible = Result<(), Box<dyn std::error::Error>>;

    fn saved(error: FeedError) -> Option<FollowupFailure> {
        Some(FollowupFailure {
            step: FollowupStep::SaveSubscription,
            error,
        })
    }

    fn unreadable() -> FeedError {
        FeedError::SubscriptionsUnreadable {
            reason: "the disk is full".to_string(),
        }
    }

    fn refreshed(outcome: &RefreshOutcome) -> String {
        let mut text = String::new();
        write_refresh(&mut text, outcome);
        text
    }

    #[test]
    fn a_clean_refresh_reports_what_changed() {
        assert_eq!(
            refreshed(&RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 412,
                skipped: 3,
                url_moved: None,
                followup: None,
            }),
            "radio-t: updated, 412 episodes retained, 3 skipped\n"
        );
        assert_eq!(
            refreshed(&RefreshOutcome::Unchanged {
                slug: "radio-t".to_string(),
                url_moved: None,
                followup: None,
            }),
            "radio-t: unchanged\n"
        );
    }

    /// §6.6: a 304 can still follow a permanent redirect, and that commit can
    /// fail on its own. The redirect is reported, the cause is named, and the
    /// URL is redacted at presentation even though the library already
    /// supplied safe text.
    #[test]
    fn a_revalidated_feed_reports_a_failed_redirect_commit() {
        let rendered = refreshed(&RefreshOutcome::Unchanged {
            slug: "radio-t".to_string(),
            url_moved: Some("https://example.org/new?token=secret".to_string()),
            followup: saved(unreadable()),
        });
        assert!(rendered.starts_with("radio-t: unchanged\n"), "{rendered}");
        assert!(rendered.contains("https://example.org/new"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(
            rendered.contains("the redirected feed URL could not be recorded"),
            "{rendered}"
        );
        assert!(rendered.contains("the disk is full"), "{rendered}");
    }

    /// Without a redirect the same step means the feed's own metadata
    /// changed, and saying "redirect" there would describe something that
    /// did not happen.
    #[test]
    fn a_failed_metadata_commit_does_not_claim_a_redirect() {
        let rendered = refreshed(&RefreshOutcome::Updated {
            slug: "radio-t".to_string(),
            retained: 5,
            skipped: 0,
            url_moved: None,
            followup: saved(unreadable()),
        });
        assert!(!rendered.contains("redirect"), "{rendered}");
        assert!(
            rendered.contains("the feed's changed title could not be recorded"),
            "{rendered}"
        );
    }

    fn mixed_batch() -> Vec<RefreshOutcome> {
        vec![
            RefreshOutcome::Updated {
                slug: "radio-t".to_string(),
                retained: 2,
                skipped: 0,
                url_moved: None,
                followup: None,
            },
            RefreshOutcome::Failed {
                slug: "sysdesign".to_string(),
                error: FeedError::UnsupportedFormat,
            },
            RefreshOutcome::Unchanged {
                slug: "late".to_string(),
                url_moved: None,
                followup: saved(unreadable()),
            },
        ]
    }

    /// §6.4: one bad feed neither hides the others nor completes. Every
    /// outcome is reported, and the error counts the feeds rather than
    /// naming one.
    #[test]
    fn a_mixed_batch_prints_everything_then_reports_the_count() -> Fallible {
        let report = finish_refresh_batch(mixed_batch());
        let error = report
            .outcome
            .err()
            .ok_or("a batch with two bad feeds must not succeed")?;
        assert!(report.text.contains("radio-t: updated"), "{}", report.text);
        assert!(report.text.contains("sysdesign: failed"), "{}", report.text);
        assert!(report.text.contains("late: unchanged"), "{}", report.text);
        assert!(
            matches!(
                error,
                FeedError::BatchIncomplete {
                    failed: 2,
                    total: 3
                }
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn a_clean_batch_exits_zero() -> Fallible {
        let report = finish_refresh_batch(vec![RefreshOutcome::Unchanged {
            slug: "radio-t".to_string(),
            url_moved: None,
            followup: None,
        }]);
        report.outcome?;
        assert_eq!(report.text, "radio-t: unchanged\n");

        let empty = finish_refresh_batch(Vec::new());
        empty.outcome?;
        assert!(empty.text.is_empty());
        Ok(())
    }

    /// §6.4: a single-feed command returns the concrete error — not a batch
    /// count — after reporting what did commit.
    #[test]
    fn one_feed_returns_its_own_error_after_printing() -> Fallible {
        let report = finish_refresh_one(RefreshOutcome::Failed {
            slug: "radio-t".to_string(),
            error: FeedError::UnsupportedFormat,
        });
        assert!(report.text.contains("radio-t: failed"), "nothing printed");
        let error = report
            .outcome
            .err()
            .ok_or("a failed refresh must not succeed")?;
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");

        let report = finish_refresh_one(RefreshOutcome::Updated {
            slug: "radio-t".to_string(),
            retained: 2,
            skipped: 0,
            url_moved: None,
            followup: saved(unreadable()),
        });
        assert!(
            report.text.contains("radio-t: updated, 2 episodes"),
            "{}",
            report.text
        );
        let error = report
            .outcome
            .err()
            .ok_or("a followup failure must not succeed")?;
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    fn feed_id() -> Result<crate::media::id::FeedId, Box<dyn std::error::Error>> {
        Ok(crate::media::id::FeedId::new(
            "0123456789abcdef0123456789abcdef".to_string(),
        )?)
    }

    #[test]
    fn subscribing_prints_its_slug_and_counts() -> Fallible {
        let report = finish_subscribe(SubscribeOutcome {
            slug: "radio-t".to_string(),
            feed_id: feed_id()?,
            title: Some("Радио-Т".to_string()),
            retained: 412,
            skipped: 3,
            followup: None,
        });
        report.outcome?;
        assert_eq!(
            report.text,
            "radio-t: subscribed, 412 episodes retained, 3 skipped\n"
        );
        Ok(())
    }

    /// §5.3's commit order is cache first, subscription second, so a
    /// followup failure here means nothing is subscribed — and the line must
    /// say so rather than reporting a subscription that does not exist.
    #[test]
    fn a_half_committed_subscribe_never_claims_success() -> Fallible {
        let report = finish_subscribe(SubscribeOutcome {
            slug: "radio-t".to_string(),
            feed_id: feed_id()?,
            title: None,
            retained: 412,
            skipped: 0,
            followup: saved(unreadable()),
        });
        let rendered = &report.text;
        assert!(!rendered.contains("radio-t: subscribed,"), "{rendered}");
        assert!(rendered.contains("nothing is subscribed"), "{rendered}");
        assert!(rendered.contains("412 episodes were cached"), "{rendered}");
        let error = report
            .outcome
            .err()
            .ok_or("a half-committed subscribe must not succeed")?;
        assert!(
            matches!(error, FeedError::SubscriptionsUnreadable { .. }),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn unsubscribing_reports_a_cleanup_failure_as_a_failure() -> Fallible {
        let report = finish_unsubscribe(UnsubscribeOutcome {
            slug: "radio-t".to_string(),
            followup: None,
        });
        report.outcome?;
        assert_eq!(report.text, "radio-t: unsubscribed\n");

        let report = finish_unsubscribe(UnsubscribeOutcome {
            slug: "radio-t".to_string(),
            followup: Some(FollowupFailure {
                step: FollowupStep::RemoveCache,
                error: FeedError::UnsupportedFormat,
            }),
        });
        let rendered = &report.text;
        assert!(
            rendered.contains("the subscription was removed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("cached episodes could not be deleted"),
            "{rendered}"
        );
        let error = report
            .outcome
            .err()
            .ok_or("a failed cleanup must not succeed")?;
        assert!(matches!(error, FeedError::UnsupportedFormat), "{error:?}");
        Ok(())
    }

    #[test]
    fn adding_a_verified_station_prints_its_identity() -> Fallible {
        let report = finish_add_station(AddStationOutcome::Verified {
            slug: "test-radio".to_string(),
            identity: StationIdentity {
                name: Some("Test Radio".to_string()),
                genre: Some("Lofi".to_string()),
                bitrate_kbps: Some(128),
                logo: None,
            },
        });
        report.outcome?;
        assert_eq!(
            report.text,
            "test-radio: added, verified — Test Radio · Lofi · 128 kbps\n"
        );
        Ok(())
    }

    /// `AddStationOutcome::AlreadySaved::reprobe_failure` exists so that a
    /// duplicate add's implicit re-probe failure reaches the caller instead
    /// of being swallowed by the "already saved" framing (M7.1 §6, §10); this
    /// is the presentation-layer half of that fix — the reason must show up
    /// in the reported line, not just in the value passed to
    /// `finish_add_station`.
    #[test]
    fn a_duplicate_adds_failed_reprobe_is_not_swallowed() -> Fallible {
        let report = finish_add_station(AddStationOutcome::AlreadySaved {
            slug: "test-radio".to_string(),
            identity: Some(StationIdentity {
                name: Some("Test Radio".to_string()),
                genre: None,
                bitrate_kbps: None,
                logo: None,
            }),
            reprobe_failure: Some("connection reset".to_string()),
        });
        let rendered = report.text.clone();
        // Not just the text: `into_notice` and the browse worker's log only
        // mark a failure, and the browser (`src/tui/browser.rs`) only paints
        // `NoticeKind::Err`, when the outcome is `Err` — exactly like
        // `finish_subscribe`'s and `finish_unsubscribe`'s own partial-failure
        // arms. A `contains()` check on the string alone would pass even if
        // the outcome were `Ok`.
        let error = report
            .outcome
            .err()
            .ok_or("a failed implicit re-probe must not report success")?;
        assert!(
            matches!(error, FeedError::StationsUnreadable { .. }),
            "{error:?}"
        );
        assert!(rendered.contains("already saved"), "{rendered}");
        assert!(
            rendered.contains("re-probe failed: connection reset"),
            "the re-probe failure must be surfaced, not swallowed: {rendered}"
        );
        assert!(
            rendered.contains("Test Radio"),
            "the station's prior identity is still shown: {rendered}"
        );
        Ok(())
    }

    #[test]
    fn removing_a_station_prints_its_slug() -> Fallible {
        let report = finish_remove_station(RemoveStationOutcome {
            slug: "test-radio".to_string(),
        });
        report.outcome?;
        assert_eq!(report.text, "test-radio: removed\n");
        Ok(())
    }

    /// The browser's notice is today's `commands::report` text: trailing
    /// whitespace trimmed, then the text alone, the error alone, or the text
    /// and the error on the next line.
    #[test]
    fn a_report_becomes_the_notice_the_browser_shows() {
        let complete = Report {
            text: "radio-t: unchanged\n \n".to_string(),
            outcome: Ok(()),
        };
        assert_eq!(complete.into_notice(), Ok("radio-t: unchanged".to_string()));

        let early = Report {
            text: String::new(),
            outcome: Err(FeedError::UnsupportedFormat),
        };
        assert_eq!(
            early.into_notice(),
            Err(FeedError::UnsupportedFormat.to_string())
        );

        let partial = Report {
            text: "radio-t: unsubscribed  \n".to_string(),
            outcome: Err(FeedError::UnsupportedFormat),
        };
        assert_eq!(
            partial.into_notice(),
            Err(format!(
                "radio-t: unsubscribed\n{}",
                FeedError::UnsupportedFormat
            ))
        );
    }

    /// Review focus 1: a refresh-all with failures is one red notice that
    /// lists every feed, then the count.
    #[test]
    fn a_mixed_batch_becomes_one_failed_notice() -> Fallible {
        let notice = finish_refresh_batch(mixed_batch())
            .into_notice()
            .err()
            .ok_or("a batch with failures must be a failed notice")?;
        let count = FeedError::BatchIncomplete {
            failed: 2,
            total: 3,
        }
        .to_string();
        let lines: Vec<&str> = notice.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some("radio-t: updated, 2 episodes retained, 0 skipped")
        );
        assert_eq!(lines.last().copied(), Some(count.as_str()));
        Ok(())
    }

    /// Review focus 2: an unsubscribe or a station removal is a local edit.
    /// Even when it fails, it never spawns the HTTP service. It also fails
    /// before it has anything to report, so its text is empty.
    #[test]
    fn local_operations_leave_the_http_slot_empty() -> Fallible {
        let dir = tempfile::tempdir()?;
        let stores = LibraryStores {
            subscriptions: SubscriptionStore::new(
                dir.path().join("subscriptions.json"),
                Arc::new(SystemClock),
            ),
            cache: CacheStore::new(dir.path().join("feeds")),
            stations: StationStore::new(dir.path().join("stations.json"), Arc::new(SystemClock)),
        };
        let mut http = None;
        for op in [
            FeedOp::Unsubscribe {
                slug: "nope".to_string(),
            },
            FeedOp::RemoveStation {
                slug: "nope".to_string(),
            },
        ] {
            let report = run(&op, &stores, &mut http);
            assert!(report.text.is_empty(), "{op:?}: {}", report.text);
            assert!(
                matches!(report.outcome, Err(FeedError::UnknownSlug { .. })),
                "{op:?}: {:?}",
                report.outcome
            );
            assert!(http.is_none(), "{op:?} spawned an HTTP service");
        }
        Ok(())
    }
}
```

If `FeedError` does not implement `PartialEq`, `assert_eq!` on `into_notice()` still works, because it returns `Result<String, String>`. If the station store's constructor takes the clock differently from the subscription store's, follow its signature in `src/station/store.rs`.

- [ ] **Step 4: Run the new module's tests**

Run: `cargo test --locked --lib application::feed_ops`
Expected: PASS for all 15 tests. Nothing calls `feed_ops::run` yet and the old `commands` items still exist alongside it (different modules, so no name clash), so the build warns about dead code until Steps 5 and 7 wire it in. That's expected at this point.

- [ ] **Step 5: Rewrite `commands.rs`'s mutation arms and delete the moved items**

Delete from `src/commands.rs`: `wait_http`, `report`, `ABSENT`, `write_refresh`, `finish_refresh_batch`, `finish_refresh_one`, `finish_subscribe`, `finish_unsubscribe`, `station_identity_text`, `write_station_probe`, `finish_add_station`, `finish_reprobe_station`, `finish_remove_station`, and from `mod tests` the twelve tests moved in Step 3 together with their helpers `saved` and `unreadable`, if nothing else in `commands.rs`'s tests uses them. Import `ABSENT` with `use crate::application::feed_ops::{self, ABSENT, FeedOp, Report};`.

In `run`, the `Subscribe`, `Unsubscribe` and both `Refresh` arms become:

```rust
        CliCommand::Subscribe { url, slug } => {
            operate(&mut out, &FeedOp::Subscribe { url, slug })?
        }
        CliCommand::Unsubscribe { slug } => operate(&mut out, &FeedOp::Unsubscribe { slug })?,
        CliCommand::Refresh { slug } => operate(&mut out, &FeedOp::Refresh { slug })?,
```

The `Unsubscribe` arm's comment about not spawning an `HttpService` moves to `feed_ops::run`'s doc, which already says it.

Add, after `stdout_failure`:

```rust
/// A feed operation, printed. The outer `Err` is an operation that failed
/// before it had anything to print, as a store, spawn or library call can:
/// it returns before the final flush, as it always has. The inner result is
/// the printed report's: a stdout failure outranks the operation's own
/// error, and the final flush outranks both.
fn operate(out: &mut dyn Write, op: &FeedOp) -> Result<Result<(), FeedError>, FeedError> {
    let report = feed_ops::run(op, &LibraryStores::platform()?, &mut None);
    if report.text.is_empty()
        && let Err(error) = report.outcome
    {
        return Err(error);
    }
    Ok(write_report(out, report))
}

/// The report's text on stdout, then its outcome. A command that could not
/// report its own result has not succeeded, however far its work got.
fn write_report(out: &mut dyn Write, report: Report) -> Result<(), FeedError> {
    out.write_all(report.text.as_bytes())
        .map_err(stdout_failure)
        .and(report.outcome)
}
```

If the borrow checker rejects the `if … && let Err(error) = report.outcome` move, write it as:

```rust
    match report.outcome {
        Err(error) if report.text.is_empty() => Err(error),
        outcome => Ok(write_report(out, Report { text: report.text, outcome })),
    }
```

Update the module doc at the top of `commands.rs`. It now says that `commands` formats the listing columns and prints a feed operation's report and exit status, and that the operations themselves, and `wait_http`, live in [`crate::application::feed_ops`]. Replace its last sentence ("`wait_http` is shared with the browse worker; …") with:

```rust
//! keeps `block_on` out of `library`. The operations themselves, and the
//! one synchronous bridge into the runtime, are
//! [`crate::application::feed_ops`]'s; the artwork worker blocks on its own
//! (architecture §5).
```

Also update `src/app.rs:68-70`'s doc on `run`: "which owns every line this program prints for a feed command, the one synchronous bridge into the HTTP runtime, and the exit status a partial failure has to carry" becomes "which prints every line a feed command reports and maps a partial failure to a failing exit status".

Remove the imports that are now unused (`Limits`, `HttpService`, `StationIdentity`, `AddStationOutcome`, `FollowupStep`, `RefreshOutcome`, `RemoveStationOutcome`, `SubscribeOutcome`, `UnsubscribeOutcome`, `redact_url` if only `write_refresh` used it). Clippy names them.

- [ ] **Step 6: Adapt the broken-stdout test**

In `commands.rs`'s `an_unwritable_stream_fails_the_command`, the `write_feeds(&mut Broken, …)` half stays as it is. Replace the trailing `finish_unsubscribe(&mut Broken, …)` assertion with one that shows the precedence (review focus 3):

```rust
        // A report whose operation also failed: the stdout failure is what
        // the command reports, as it always was.
        let error = write_report(
            &mut Broken,
            Report {
                text: "radio-t: unsubscribed\n".to_string(),
                outcome: Err(FeedError::UnsupportedFormat),
            },
        )
        .err()
        .ok_or("a broken writer must not report success")?;
        assert_eq!(
            error.to_string(),
            "cannot write command output to \"<stdout>\""
        );
        Ok(())
```

Drop the test module's imports that are now unused (`SubscribeOutcome`, `UnsubscribeOutcome`, `FollowupFailure`, `FollowupStep`, `RefreshOutcome`).

- [ ] **Step 7: Point `browse.rs` at `feed_ops`, with a temporary conversion**

In `src/application/browse.rs`, replace the `use crate::commands::{…}` block and the mutation names in the `use crate::library::{…}` block, so `library` imports only `EpisodeCandidate, FeedSummary, StationRow, episode_candidates, list_feeds, list_stations`. Add:

```rust
use crate::application::feed_ops::{self, FeedOp};
```

Replace `mutate` and delete `http_service`:

```rust
/// Runs one mutation the way its CLI command does, and reports it the way
/// the CLI prints it (§3). The service in `http` is spawned on first use,
/// and only by an operation that needs the network (`feed_ops::run`).
fn mutate(
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
    request: &BrowseRequest,
) -> Result<String, String> {
    // Temporary: Task 4 replaces this conversion with `BrowseRequest::Op`.
    let op = match request {
        BrowseRequest::Subscribe { url } => FeedOp::Subscribe {
            url: url.clone(),
            slug: None,
        },
        BrowseRequest::Refresh { slug } => FeedOp::Refresh { slug: slug.clone() },
        BrowseRequest::Unsubscribe { slug } => FeedOp::Unsubscribe { slug: slug.clone() },
        BrowseRequest::AddStation { url } => FeedOp::AddStation { url: url.clone() },
        BrowseRequest::RemoveStation { slug } => FeedOp::RemoveStation { slug: slug.clone() },
        BrowseRequest::ReprobeStation { slug } => FeedOp::ReprobeStation { slug: slug.clone() },
        BrowseRequest::Directory(_)
        | BrowseRequest::Feeds
        | BrowseRequest::Episodes { .. }
        | BrowseRequest::Stations
        | BrowseRequest::CollectTree { .. } => return Err("not a mutation".to_owned()),
    };
    feed_ops::run(&op, stores, http).into_notice()
}
```

Also update the module doc's sentence "the feed listings are the same read-only snapshot reads `tenuto feeds` uses" only if it names `commands`; it doesn't today, so leave it. Remove the now-unused `Limits` import.

- [ ] **Step 8: Fix the diagnostics table path**

`tests/m4_diagnostics.rs:649`: `` `commands::finish_refresh_batch` `` becomes `` `application::feed_ops::finish_refresh_batch` ``.

- [ ] **Step 9: Run the layering test and the mutation suites**

Run: `cargo test --locked --test m9_layering --test m5_browser --test m6_feed_management --test m7_1_station_probe --test m4_diagnostics --test m4_cli && cargo test --locked --lib`
Expected: PASS. `ALLOWED` is empty and nothing reports an edge.

- [ ] **Step 10: Gate**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add -A src tests
git commit -m "refactor(m9): one feed-operation dispatch below commands; ALLOWED is empty"
```

---

### Task 4: `BrowseRequest::Op(FeedOp)`

**Files:**
- Modify: `src/application/browse.rs` (enum, `serve`, `mutate`, `describe`; new `mod tests`)
- Modify: `src/tui/browser.rs:19,239,318-319,361-362,373,450-456,500-501`
- Modify: `tests/m5_browser.rs` (21 sites), `tests/m6_feed_management.rs` (13), `tests/m7_1_station_probe.rs` (6)

**Interfaces:**
- Consumes: `FeedOp`, `feed_ops::run`, `Report::into_notice` (Task 3).
- Produces: `BrowseRequest::Op(FeedOp)` in place of `Subscribe`, `Refresh`, `Unsubscribe`, `AddStation`, `RemoveStation` and `ReprobeStation`. The listing variants (`Directory`, `Feeds`, `Episodes`, `Stations`, `CollectTree`) are unchanged.

**The edit rule**, used at every site in this task:
- `BrowseRequest::Subscribe { url: X }` → `BrowseRequest::Op(FeedOp::Subscribe { url: X, slug: None })`
- `BrowseRequest::Subscribe { .. }` → `BrowseRequest::Op(FeedOp::Subscribe { .. })`
- `BrowseRequest::V { … }` for `V` in `Refresh`, `Unsubscribe`, `AddStation`, `RemoveStation`, `ReprobeStation` → `BrowseRequest::Op(FeedOp::V { … })`, with the fields unchanged.
- Each file that gains `FeedOp` imports it: `tenuto::application::feed_ops::FeedOp` in `tests/`, `crate::application::feed_ops::FeedOp` in `src/`.

No assertion changes.

- [ ] **Step 1: Write the failing redaction test in `browse.rs`**

Append to `src/application/browse.rs` (it has no `mod tests` today):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Review focus 4: a feed or station URL can carry userinfo or a signed
    /// query, and the worker's log line must not (§7.2).
    #[test]
    fn describe_redacts_feed_and_station_urls() {
        let url = "https://user:pw@example.org/feed?token=secret".to_string();
        assert_eq!(
            describe(&BrowseRequest::Op(FeedOp::Subscribe {
                url: url.clone(),
                slug: None,
            })),
            "Subscribe(https://example.org/feed)"
        );
        assert_eq!(
            describe(&BrowseRequest::Op(FeedOp::AddStation { url })),
            "AddStation(https://example.org/feed)"
        );
        assert_eq!(
            describe(&BrowseRequest::Op(FeedOp::Refresh { slug: None })),
            "Refresh(all)"
        );
    }
}
```

- [ ] **Step 2: Apply the edit rule to the three test suites**

Edit `tests/m5_browser.rs`, `tests/m6_feed_management.rs` and `tests/m7_1_station_probe.rs` by the rule above. Find every site with:

```bash
grep -n "BrowseRequest::\(Subscribe\|Refresh\|Unsubscribe\|AddStation\|RemoveStation\|ReprobeStation\)" tests/m5_browser.rs tests/m6_feed_management.rs tests/m7_1_station_probe.rs
```

Expected: 40 lines (21 + 13 + 6). A construction that spans several lines needs its closing `}` turned into `})`.

- [ ] **Step 3: Verify the build fails**

Run: `cargo test --locked --no-run --test m5_browser --lib`
Expected: FAIL with `no variant or associated item named `Op` found for enum `BrowseRequest``.

- [ ] **Step 4: Replace the six variants in `BrowseRequest`**

In `src/application/browse.rs`, delete the `Subscribe`, `Refresh`, `Unsubscribe`, `AddStation`, `RemoveStation` and `ReprobeStation` variants with their doc comments (`FeedOp` carries them now). After `Stations`, add:

```rust
    /// A change to the feed library or the saved stations, run the way its
    /// CLI command runs it ([`feed_ops::run`]). Only `Subscribe`,
    /// `Refresh`, `AddStation` and `ReprobeStation` touch the network.
    Op(FeedOp),
```

In `serve`, the mutation arm's pattern becomes `request @ BrowseRequest::Op(_)`, and the body is unchanged.

`mutate` loses the Task 3 conversion:

```rust
/// Runs one mutation the way its CLI command does, and reports it the way
/// the CLI prints it (§3). The service in `http` is spawned on first use,
/// and only by an operation that needs the network (`feed_ops::run`).
fn mutate(
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
    request: &BrowseRequest,
) -> Result<String, String> {
    match request {
        BrowseRequest::Op(op) => feed_ops::run(op, stores, http).into_notice(),
        BrowseRequest::Directory(_)
        | BrowseRequest::Feeds
        | BrowseRequest::Episodes { .. }
        | BrowseRequest::Stations
        | BrowseRequest::CollectTree { .. } => Err("not a mutation".to_owned()),
    }
}
```

`describe` keeps every string it produces today:

```rust
fn describe(request: &BrowseRequest) -> String {
    match request {
        BrowseRequest::Directory(_) => "Directory".to_string(),
        BrowseRequest::Feeds => "Feeds".to_string(),
        BrowseRequest::Episodes { slug } => format!("Episodes({})", slug),
        BrowseRequest::Stations => "Stations".to_string(),
        BrowseRequest::Op(FeedOp::Subscribe { url, .. }) => {
            format!("Subscribe({})", redact_url(url))
        }
        BrowseRequest::Op(FeedOp::Refresh { slug: Some(slug) }) => format!("Refresh({})", slug),
        BrowseRequest::Op(FeedOp::Refresh { slug: None }) => "Refresh(all)".to_string(),
        BrowseRequest::Op(FeedOp::Unsubscribe { slug }) => format!("Unsubscribe({})", slug),
        // A station URL can carry userinfo exactly as a feed URL can, so it
        // is redacted here for the same reason `Subscribe` is (§7.2).
        BrowseRequest::Op(FeedOp::AddStation { url }) => {
            format!("AddStation({})", redact_url(url))
        }
        BrowseRequest::Op(FeedOp::RemoveStation { slug }) => format!("RemoveStation({})", slug),
        BrowseRequest::Op(FeedOp::ReprobeStation { slug }) => {
            format!("ReprobeStation({})", slug)
        }
        BrowseRequest::CollectTree { roots, .. } => format!("CollectTree({} roots)", roots.len()),
    }
}
```

Check the module doc's line "Only an explicit `Subscribe`, `Refresh`, `AddStation` or `ReprobeStation` request touches the network." It stays true as written.

- [ ] **Step 5: Apply the edit rule to `src/tui/browser.rs`**

Add `use crate::application::feed_ops::FeedOp;`. Then edit the 14 sites:
- `:239` `if let BrowseRequest::Unsubscribe { slug } = &request` → `if let BrowseRequest::Op(FeedOp::Unsubscribe { slug }) = &request`
- `:318-319`, `:361-362`, `:373`: constructions, by the rule.
- `:450-456`: the `submit` label match arms, by the rule (`BrowseRequest::Op(FeedOp::Subscribe { .. }) => "Subscribing…"`, …).
- `:500-501`: `BrowseRequest::Op(FeedOp::AddStation { url })` and `BrowseRequest::Op(FeedOp::Subscribe { url, slug: None })`.

- [ ] **Step 6: Run the affected suites**

Run: `cargo test --locked --lib application::browse && cargo test --locked --test m5_browser --test m6_feed_management --test m7_1_station_probe --test m8_browser --test m5_no_network --test m9_layering`
Expected: PASS, including `describe_redacts_feed_and_station_urls`.

- [ ] **Step 7: Gate**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast`
Expected: all pass. `grep -rn "BrowseRequest::\(Subscribe\|Refresh\|Unsubscribe\|AddStation\|RemoveStation\|ReprobeStation\)" src tests` prints nothing.

- [ ] **Step 8: Commit**

```bash
git add -A src tests
git commit -m "refactor(m9): the browser sends a FeedOp, one operation type for both front ends"
```

---

### Task 5: Documentation, parity run, acceptance record

**Files:**
- Modify: `docs/architecture.md` (§4 diagram, component table, "Known layering exceptions"; §12 M9 bullet)
- Modify: `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` (§5 status line, last)
- Modify: `CHANGELOG.md` (`[Unreleased]` → `### Internal`)
- Create: `docs/m9-c-acceptance.md`

**Interfaces:** none.

- [ ] **Step 1: Confirm the `--as` coverage**

Run: `grep -n '"--as"' tests/m4_cli.rs`
Expected: the subprocess helper `subscribe(root, server)` runs `subscribe <url> --as radio-t` (around line 412). Record that in the acceptance record. The parity run's scenario 1 uses `--as` as well.

- [ ] **Step 2: Update `docs/architecture.md` §4**

In the mermaid diagram:
- the `commands` node's label becomes `commands.rs<br/>feed listing columns,<br/>report printing and exit status`;
- in the `appl` subgraph, after `library`, add `feed_ops["application/feed_ops.rs<br/>feed and station mutations,<br/>report and outcome, wait_http"]`;
- add the edges `commands --> feed_ops`, `workers --> feed_ops` and `feed_ops --> library`. Keep `commands --> library` (the listings) and `workers --> library` (the browse listings).

In the component table, the `commands` row becomes:

```
| `commands` | Format feed listing columns. Print a feed operation's report and map its outcome to the exit status. | Decide what to fetch or commit, or whether an operation completed |
```

Add a row after `library`:

```
| `application::feed_ops` | The one dispatch for feed and station mutations, shared by the CLI and the browse worker: the report text and a complete-or-incomplete outcome. Owns `wait_http`, the synchronous bridge into the Tokio runtime. | Format listing columns, print, or decide exit codes |
```

Replace the "**Known layering exceptions.**" paragraph and its two bullets with:

```
**Remaining structural debt.** The test's `ALLOWED` list is empty: no module imports above its rank. One M9.2 item is architectural rather than an import exception: both front ends load `StateStore` and take the profile lock themselves at startup; `application::profile::open_state` then builds the `Session` and writer for each.
```

- [ ] **Step 3: Update `docs/architecture.md` §12's M9 bullet**

In "**Architecture deepening (M9).**", delete "feed operations below `commands` (M9.3); " from the list of open items. Replace the last two sentences ("M9.2's mirror and M9.3's `play` item have shipped: … `tests/m9_layering.rs` checks the layering of §4 and holds the exceptions still to remove.") with:

```
M9.3 has shipped: `play` is a front end over `PlayerRuntime`, so there is one display mirror and one state opener, and its detached load (`LoadTarget::Detached`) leaves the playlists alone; feed and station mutations run through one `application::feed_ops` dispatch below `commands`. `tests/m9_layering.rs` checks the layering of §4 with no exceptions. The M9.2 startup item (§4, "Remaining structural debt") is still open, as architectural debt rather than an import exception.
```

- [ ] **Step 4: Add the CHANGELOG entry**

At the end of `CHANGELOG.md`'s `[Unreleased]` → `### Internal` list, hard-wrapped like its neighbours:

```markdown
- Feed and station changes run through one operation, shared by `tenuto
  subscribe`/`unsubscribe`/`refresh` and the player's browser. Output, exit
  codes and notices are unchanged. Library API: `BrowseRequest`'s
  `Subscribe`, `Refresh`, `Unsubscribe`, `AddStation`, `RemoveStation` and
  `ReprobeStation` variants are now `BrowseRequest::Op(FeedOp)`.
```

- [ ] **Step 5: Run the gate and record it**

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast 2>&1 | tee "$SCRATCH/test.log"
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

(`$SCRATCH` is any scratch directory outside the repo.) Count the result lines from `test.log`: `grep -c "^test result:"`, and sum the passed, failed and ignored counts.

- [ ] **Step 6: Run the manual CLI parity comparison**

This compares `main` (`5582b5c`) with the branch head. Every scenario starts both sides from the same seeded profile, and stderr has `tracing`'s timestamp prefix stripped before the comparison.

```bash
set -u
ROOT=$(mktemp -d)
git worktree add "$ROOT/main" 5582b5c
(cd "$ROOT/main" && cargo build --release --locked)
cargo build --release --locked
MAIN="$ROOT/main/target/release/tenuto"
BRANCH="$PWD/target/release/tenuto"

mkdir -p "$ROOT/www"
cp tests/fixtures/feeds/rss2-minimal.xml "$ROOT/www/a.xml"
cp tests/fixtures/feeds/atom-minimal.xml "$ROOT/www/b.xml"
python3 -m http.server 8765 --bind 127.0.0.1 -d "$ROOT/www" >/dev/null 2>&1 &
SERVER=$!
sleep 1   # the server's own startup; nothing under test is timed
URL=http://127.0.0.1:8765

# tenuto against profile directory $1, arguments after it.
in_profile() { local dir=$1; shift; XDG_DATA_HOME="$dir/data" XDG_CACHE_HOME="$dir/cache" "$@"; }

# Record stdout, timestamp-stripped stderr and the exit code of one call.
record() { # side step profile args...
  local side=$1 step=$2 dir=$3; shift 3
  local bin; [ "$side" = main ] && bin=$MAIN || bin=$BRANCH
  mkdir -p "$ROOT/out/$side"
  in_profile "$dir" "$bin" "$@" >"$ROOT/out/$side/$step.out" 2>"$ROOT/out/$side/$step.err.raw"
  echo $? >"$ROOT/out/$side/$step.code"
  sed -E 's/^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z +//' \
    "$ROOT/out/$side/$step.err.raw" >"$ROOT/out/$side/$step.err"
  rm "$ROOT/out/$side/$step.err.raw"
}

# A fresh copy of template $1 as side $2's profile.
fresh() { rm -rf "$ROOT/p/$2"; mkdir -p "$ROOT/p"; cp -a "$1" "$ROOT/p/$2"; }

# Templates, seeded once with the main binary.
mkdir -p "$ROOT/t/empty/data" "$ROOT/t/empty/cache"
cp -a "$ROOT/t/empty" "$ROOT/t/two"
in_profile "$ROOT/t/two" "$MAIN" subscribe "$URL/a.xml" --as alpha
in_profile "$ROOT/t/two" "$MAIN" subscribe "$URL/b.xml" --as beta

for side in main branch; do
  # 1. success: subscribe with --as, refresh it, unsubscribe it
  fresh "$ROOT/t/empty" $side
  record $side 1a "$ROOT/p/$side" subscribe "$URL/a.xml" --as alpha
  record $side 1b "$ROOT/p/$side" refresh alpha
  record $side 1c "$ROOT/p/$side" unsubscribe alpha
  # 2. early failure, then the same with stdout closed
  fresh "$ROOT/t/empty" $side
  record $side 2a "$ROOT/p/$side" refresh no-such-slug
  in_profile "$ROOT/p/$side" "$( [ $side = main ] && echo "$MAIN" || echo "$BRANCH")" \
    refresh no-such-slug >&- 2>"$ROOT/out/$side/2b.err.raw"
  echo $? >"$ROOT/out/$side/2b.code"
  sed -E 's/^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z +//' "$ROOT/out/$side/2b.err.raw" >"$ROOT/out/$side/2b.err"
  rm "$ROOT/out/$side/2b.err.raw"
  # 3. partial failure: refresh-all with beta's URL gone
  fresh "$ROOT/t/two" $side
  mv "$ROOT/www/b.xml" "$ROOT/www/b.gone"
  record $side 3 "$ROOT/p/$side" refresh
  mv "$ROOT/www/b.gone" "$ROOT/www/b.xml"
done

kill $SERVER
diff -r "$ROOT/out/main" "$ROOT/out/branch" && echo PARITY
head -n 5 "$ROOT"/out/branch/*.out "$ROOT"/out/branch/*.err "$ROOT"/out/branch/*.code
git worktree remove "$ROOT/main"
```

Expected: `PARITY`. Then check that the scenarios exercised what they claim:
- `1a.code`, `1b.code` and `1c.code` are `0`;
- `2a.out` is empty, `2a.code` is `1`, and `2a.err` names the unknown slug;
- `2b.err` names the unknown slug too, not a stdout failure;
- `3.out` has an `alpha:` line and a `beta: failed:` line, `3.code` is `1`, and `3.err` carries the `BatchIncomplete` message.

If `diff` reports a difference, stop: the refactor changed behavior. Find the cause, and don't adjust the script to hide it.

- [ ] **Step 7: Write `docs/m9-c-acceptance.md`**

Follow `docs/m9.3-acceptance.md`'s shape:

```markdown
# M9 PR C: feed operations below `commands` — acceptance

Scope: the M9.3 "Feed operations below `commands.rs`" item of
`docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §5.
Spec: `docs/superpowers/specs/2026-10-09-tenuto-m9-feed-operations-design.md`.
Plan: `docs/superpowers/plans/2026-10-09-m9-feed-operations.md`.

Branch `refactor/m9-feed-operations`, from `main` at `5582b5c`.

## Decisions

Made with the user on 2026-10-09:

- `displayable` moves to `telemetry`, beside `redact_url`.
- One `FeedOp` and one `feed_ops::run`. `BrowseRequest` carries
  `Op(FeedOp)`; `FeedOp` is public, and `Report` and `run` are crate-private.
- The CLI keeps today's failure order: an early error returns before the
  flush, a stdout failure outranks the operation's error, and a flush
  failure outranks both.
- The public `BrowseRequest` change ships without a shim, recorded under
  `### Internal` in the CHANGELOG.

## Gate

Run on this branch after the last code change.

| Check | Command | Result |
|---|---|---|
| Format | `cargo fmt --check` | <result> |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | <result> |
| Tests | `cargo test --locked --no-fail-fast` | <N result lines: P passed, F failed, I ignored> |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | <result> |

## Layering

`tests/m9_layering.rs` passes with `ALLOWED` empty.

## CLI parity

`main` at `5582b5c` against the branch head at `<sha>`, with separate,
identically seeded `XDG_DATA_HOME`/`XDG_CACHE_HOME` per side, and both served
the same fixtures (`rss2-minimal.xml`, `atom-minimal.xml`) from one local
server. stdout, stderr (timestamps stripped) and exit codes compared byte for
byte with `diff -r`:

| Scenario | Commands | Result |
|---|---|---|
| 1 success | `subscribe <url> --as alpha`, `refresh alpha`, `unsubscribe alpha` | <identical; exit 0 ×3> |
| 2a early failure | `refresh no-such-slug` | <identical; no stdout; exit 1> |
| 2b early failure, stdout closed | `refresh no-such-slug >&-` | <identical; the unknown-slug error, not a stdout failure> |
| 3 partial failure | `refresh` with `beta`'s URL returning 404 | <identical; alpha line, beta failed line; BatchIncomplete; exit 1> |

`--as` coverage in the existing suites: `tests/m4_cli.rs` runs `subscribe <url> --as radio-t` as a subprocess (<line>).
```

Fill each `<…>` with what Steps 1, 5 and 6 actually printed. Don't leave a placeholder in the committed file.

- [ ] **Step 8: Mark C shipped in the deepening spec**

In `docs/superpowers/specs/2026-09-29-tenuto-m9-architecture-deepening.md` §5, under "**Feed operations below `commands.rs`.**", after its "Direction" bullet, add:

```markdown
- Status: shipped (C). `application::feed_ops` runs every feed and station mutation for both front ends and returns the report text with a complete-or-incomplete outcome; `commands` keeps the listing columns and maps the outcome to the exit status. `displayable` lives in `telemetry` and the platform stores come from `LibraryStores::platform`. `tests/m9_layering.rs` has no exceptions (`docs/m9-c-acceptance.md`).
```

- [ ] **Step 9: Commit**

```bash
git add docs CHANGELOG.md
git commit -m "docs(m9): feed operations shipped; architecture, changelog and acceptance record"
```
