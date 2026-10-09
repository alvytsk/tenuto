# Tenuto M9: feed operations below `commands`

Status: approach approved in conversation on 2026-10-09; this written spec awaits review.

Branch: `refactor/m9-feed-operations`, from `main` at `5582b5c`.

Context: PR C of the five that finish M9 ([layering test spec](2026-10-08-tenuto-m9-layering-test-design.md), its PR table). B shipped as #42 and left `tests/m9_layering.rs` with 6 allowlisted exceptions, all marked `// C`. This PR deletes them, so `ALLOWED` ends up empty. It carries out the M9.3 "Feed operations below `commands.rs`" item of the [deepening spec](2026-09-29-tenuto-m9-architecture-deepening.md) §5.

## 1. Goal

Nothing below the entry layer imports `commands`. Today `commands::run` and `application::browse::mutate` each dispatch the same feed and station mutations, and both learn whether one completed from `finish_*` + `report`. After this PR:

- one module, `application::feed_ops`, runs every feed and station mutation and returns its report text with a complete-or-incomplete outcome;
- `commands` keeps the listing columns and the mapping from outcome to stdout and exit status;
- `displayable` lives in `telemetry`, beside `redact_url`;
- the platform store constructors live on `LibraryStores`.

## 2. Compatibility boundary

Unchanged, and verified (§7):
- CLI behavior: every stdout line, the stderr line, exit codes. A partial success still prints what committed and then fails.
- The order in which a CLI feed command fails: stores, then `HttpService::spawn`, then the operation, then stdout. A stdout write failure still outranks the operation's error, and a flush failure still outranks a write failure.
- Errors: a failing feed command returns the same `FeedError` value it returns today, inside the same `AppError::Feed`. So both `main.rs`'s printed line and its `tracing::error!(error = ?error)` Debug log stay the same.
- TUI notice text, including its newline layout: `Ok(text)`, `Err(error)` when nothing was written, `Err("{text}\n{error}")` otherwise, with trailing whitespace trimmed from `text`.
- Browse worker log lines (`describe`), with URLs still passed through `redact_url`.
- Network behavior: an operation that does not need the network (`Unsubscribe`, `RemoveStation`) never spawns an `HttpService`, and the browse worker keeps reusing its one lazily spawned service.

Changed on purpose: Rust paths and signatures listed in §3–§5, and one public library type. `BrowseRequest`'s six mutation variants become `BrowseRequest::Op(FeedOp)`. The library crate's public surface exists for this repository's integration tests, so there is no compatibility shim; `CHANGELOG.md` records the break under `### Internal` (§6).

## 3. `application::feed_ops`

A new file, `src/application/feed_ops.rs`, declared in `application/mod.rs`. It sits at rank 3 inside `application`, beside `library`, not inside it: `wait_http` calls `block_on`, which `library` must never do.

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedOp {
    Subscribe { url: String, slug: Option<String> },
    Unsubscribe { slug: String },
    /// `None` refreshes every subscription.
    Refresh { slug: Option<String> },
    AddStation { url: String },
    RemoveStation { slug: String },
    ReprobeStation { slug: String },
}

pub(crate) struct Report {
    /// Exactly what the CLI prints on stdout, trailing newline included.
    pub text: String,
    /// `Err` when the operation is incomplete: the `FeedError` the CLI's exit
    /// status reports.
    pub outcome: Result<(), FeedError>,
}

pub(crate) fn run(op: &FeedOp, stores: &LibraryStores, http: &mut Option<Arc<HttpService>>) -> Report;
pub(crate) fn wait_http<F: Future>(service: &HttpService, future: F) -> F::Output;

impl Report {
    pub(crate) fn into_notice(self) -> Result<String, String>;
}
```

Visibility: `FeedOp` is `pub`, because the public `BrowseRequest` carries it and the integration tests build it. `Report`, `run`, `wait_http` and `into_notice` are `pub(crate)`.

`FeedOp`'s field names and types are the current `BrowseRequest` variants'. `Subscribe` gains `slug` for the CLI's `--slug`; the TUI passes `None`.

**Dispatch.** `run` has one arm per variant, each calling the `library` function the two dispatchers call today, with the same arguments. Six variants cover seven operations: `Refresh { slug: Some(_) }` calls `refresh`, `Refresh { slug: None }` calls `refresh_all`.

**The HTTP slot.** An arm that needs the network takes the service from `http`, spawning `HttpService::spawn(Limits::default())` into it first if it is empty. A spawn failure is that call's `FeedError::Remote`. The CLI passes a fresh `&mut None`; the browse worker passes the slot it keeps for its lifetime, so a failed spawn is retried on the next request, as today. `Unsubscribe` and `RemoveStation` never touch the slot.

**Report text.** The seven `finish_*` functions move here with `write_refresh`, `write_station_probe` and `station_identity_text`. They build `text` with `std::fmt::Write` into a `String` and return the outcome they return today, so a partial success still produces its text and an `Err`:

| Operation | Incomplete when | `outcome` |
|---|---|---|
| refresh one | `Failed`, or a followup failure | that `error` |
| refresh all | any `Failed` or followup | `BatchIncomplete { failed, total }` |
| subscribe | followup failure (subscription not saved) | that `error` |
| unsubscribe | followup failure (cache not removed) | that `error` |
| add / re-probe station | `AlreadySaved` with a failed re-probe | `StationsUnreadable { reason }` |
| remove station | never, once `remove_station` returns `Ok` | `Ok(())` |

Formatting into a `String` cannot fail, so `stdout_failure` stays in `commands`.

**Errors before any text.** When the `library` call or the spawn fails, `run` returns `Report { text: String::new(), outcome: Err(error) }` with the same `FeedError` value today's `?` propagates.

**Shared helper.** `ABSENT` (`"—"`) is used by both the station formatter and the listing columns. It moves to `feed_ops` as `pub(crate) const ABSENT`, and `commands` imports it. The other formatting helpers (`duration_text`, `timestamp_text`, `date_text`, `title_text`, the padding) serve only the listings and stay in `commands`.

**`wait_http`** moves from `commands` unchanged, with its doc comment updated to say that `feed_ops` is the one place that enters the runtime.

**`into_notice`** is today's `commands::report`: trim trailing whitespace from `text`, then `Ok(text)` when `outcome` is `Ok`, `Err(error.to_string())` when `text` is empty, otherwise `Err(format!("{text}\n{error}"))`.

## 4. Callers

### 4.1 Stores

`LibraryStores` (in `application/runtime.rs`) gains:

```rust
pub fn platform() -> Result<Self, FeedError>
```

It builds the subscription store at `data_dir/subscriptions.json`, the cache store at `cache_dir/feeds`, and the station store at `data_dir/stations.json`, each with `SystemClock`. Its one failure is the missing platform directory, which returns today's `FeedError::SubscriptionsUnreadable { reason: "no platform data directory is available" }`. Building a store touches nothing on disk, so building all three costs a command nothing.

`commands::platform_subscription_stores` and `commands::platform_station_store` are deleted. Their callers:
- `commands::run`'s `Feeds` and `Episodes` arms use `LibraryStores::platform()?` and its `subscriptions` and `cache`;
- `app.rs`'s `play <slug> <index>` resolve does the same;
- `tui/mod.rs`'s `library_stores()` becomes `LibraryStores::platform().ok()` at its two call sites, and the helper goes.

`platform_state_store` stays in `commands`, its only user.

### 4.2 `commands::run`

`Feeds` and `Episodes` are unchanged apart from §4.1. The four mutation arms become one:

```rust
let op = match command { /* Subscribe { url, slug } | Unsubscribe { slug } | Refresh { slug } */ };
let report = feed_ops::run(&op, &LibraryStores::platform()?, &mut None);
if report.text.is_empty() && report.outcome.is_err() {
    return report.outcome;
}
let outcome = write_report(&mut out, report);
// the existing final flush, then `outcome`
```

with

```rust
fn write_report(out: &mut dyn Write, report: Report) -> Result<(), FeedError> {
    out.write_all(report.text.as_bytes())
        .map_err(stdout_failure)
        .and(report.outcome)
}
```

This keeps today's precedence: an early error returns before the flush, as today's `?` does; a write error outranks the operation's error; a flush error outranks both. `write_report` is a separate function so the broken-stdout test can drive it (§7).

`wait_http`, `report` and the seven `finish_*` functions leave `commands`.

### 4.3 `application::browse`

- `BrowseRequest`'s `Subscribe`, `Refresh`, `Unsubscribe`, `AddStation`, `RemoveStation` and `ReprobeStation` variants are replaced by `Op(FeedOp)`. The listing variants are unchanged.
- `mutate` becomes `feed_ops::run(op, stores, http).into_notice()` for `Op(op)`, and keeps `Err("not a mutation")` for the rest.
- `http_service` is deleted; the lazy slot moves into `feed_ops::run`. A spawn failure's notice is unchanged, because `FeedError::Remote` is `#[error(transparent)]` over the `RemoteFailure` whose text the notice carries today.
- `describe` matches `Op(FeedOp::…)` and produces today's strings, `Subscribe` and `AddStation` through `redact_url`.
- `NO_LIBRARY` and `NOT_RUNNING` are unchanged.

### 4.4 `tui/browser.rs` and the tests

The 14 construction sites in `tui/browser.rs` become `BrowseRequest::Op(FeedOp::…)`, and the arms that react to a finished mutation match `Op(..)` the same way. The 40 sites in `tests/m5_browser.rs` (21), `tests/m6_feed_management.rs` (13) and `tests/m7_1_station_probe.rs` (6) get the same mechanical edit. No assertion changes.

## 5. `displayable` to `telemetry`

`displayable` and its private `needs_escape` move from `commands.rs` to `telemetry.rs` unchanged, doc comments included, as `pub(crate)`. Every caller imports `crate::telemetry::displayable`: `application::runtime`, `application::view`, `tui/render/browser.rs`, `lifecycle/panic.rs` and `commands` (for `title_text`).

The doc comments that name `commands::displayable` change to `telemetry::displayable`: `lifecycle/panic.rs` (the panic message comment), `tests/m4_diagnostics.rs` (module doc and the `UnsupportedEncoding` row) and `tests/m8_tui.rs`.

`commands.rs`'s title-escaping unit tests (`titles_are_defanged_without_being_rewritten`, `line_separators_and_bidi_overrides_are_escaped_too`) stay where they are, because they assert the listing tables.

## 6. Layering test and documentation

**`tests/m9_layering.rs`.** `ALLOWED` loses its six `// C` entries and their comment and becomes empty. The constant and the stale-entry check stay, so any new upward import fails the test, and `ALLOWED`'s doc comment still describes how exceptions would be listed. Nothing else changes.

**`architecture.md` §4.**
- Diagram: the `commands` node reads "feed command columns and exit status". The `application` subgraph gains `feed_ops["application/feed_ops.rs<br/>feed and station mutations,<br/>report and outcome, wait_http"]`, with edges `commands --> feed_ops`, `workers --> feed_ops` and `feed_ops --> library`.
- Component table: `commands` becomes "Format feed listing columns. Print a feed operation's report and map its outcome to the exit status." with must-not "Decide what to fetch or commit, or whether an operation completed". A new `application::feed_ops` row: "The one dispatch for feed and station mutations: the report text and a complete-or-incomplete outcome. Owns `wait_http`, the synchronous bridge into the Tokio runtime." with must-not "Format listing columns, print, or decide exit codes".
- "Known layering exceptions" becomes "Remaining structural debt". The entry-layer bullet is deleted. The text says that `ALLOWED` is empty and that the remaining M9.2 item (both front ends load `StateStore` and take the profile lock themselves) is architectural debt, not an import exception.

**`architecture.md` §12.** The M9 bullet drops "feed operations below `commands` (M9.3)" from what is still open. It says that M9.3 has fully shipped and that `tests/m9_layering.rs` now holds no exceptions. Still open are M9.1's versioned-file module and resume intent, M9.4's transport and outbox items, and the M9.2 startup item above, which is architectural rather than an import exception.

**Deepening spec §5.** The "Feed operations below `commands.rs`" item gets a "Status: shipped (C)." line, added once implementation and verification are complete.

**`CHANGELOG.md` `[Unreleased]` → `### Internal`.** One entry: the CLI and the TUI's browser run feed and station changes through one operation, with output and exit codes unchanged; in the library API, `BrowseRequest`'s mutation variants become `BrowseRequest::Op(FeedOp)`.

**Acceptance record.** `docs/m9-c-acceptance.md` records the verification in §7, with the exact commands.

## 7. Verification

**Unit tests in `feed_ops`.** The formatter tests in `commands.rs`'s `mod tests` move with the formatters (`a_clean_refresh_reports_what_changed` through `removing_a_station_prints_its_slug`, plus the `write_refresh` cases). Each now builds a `Report` from the same outcome and asserts the same strings on `text` and the same error on `outcome`. One new test covers `into_notice`. Its input text has trailing whitespace, and it checks the trimming and all three result shapes:
- `Ok` returns the trimmed text;
- `Err` with empty text returns the error alone;
- `Err` with text returns `"{trimmed text}\n{error}"`.

**Broken stdout.** `an_unwritable_stream_fails_the_command` keeps its `write_feeds(&mut Broken, …)` half unchanged. Its `finish_unsubscribe(&mut Broken, …)` half becomes `write_report(&mut Broken, report)` with a successful unsubscribe report, asserting the same stdout failure.

**Integration.** The subprocess and browser suites (`m5_browser`, `m6_feed_management`, `m7_1_station_probe`, the CLI suites) pass with only the §4.4 edits. `tests/m4_diagnostics.rs` passes with only its doc-comment edits. `tests/m9_layering.rs` passes with `ALLOWED` empty.

**Gate.** Recorded in the acceptance record with the exact commands:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

**Manual CLI parity.** Release builds from `main` (`5582b5c`) and from the branch run the same scenarios. Each binary gets its own temporary `XDG_DATA_HOME` and `XDG_CACHE_HOME`, seeded identically before each scenario, and both use the same fixture URLs, served from `tests/fixtures/feeds/` by one local HTTP server. For each scenario, stdout, stderr and the exit code must match byte for byte:
1. success: `subscribe <fixture URL>`, then `refresh <slug>`, then `unsubscribe <slug>`;
2. early failure: `refresh no-such-slug` (fails before any text);
3. partial failure: `refresh` with two subscriptions seeded, one whose URL returns 404, so one feed is printed as updated and one as failed, then `BatchIncomplete`.

`stations` has no CLI command, so the station operations are covered by the moved unit tests and `m7_1_station_probe`.

C is marked shipped (deepening spec §5, memory) only after the gate and the parity run are recorded.

## 8. Out of scope

- The M9.2 startup item (front ends loading `StateStore` and taking the profile lock). PR D or later.
- Changing any report wording or exit code, or adding CLI station commands.
- Moving the listing formatters (`write_feeds`, `write_episodes`) out of `commands`.
- `application::podcast`'s lookup by `MediaId` (deepening spec §4, speculative).
