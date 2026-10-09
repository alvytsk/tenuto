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
| Format | `cargo fmt --check` | exit 0, no output |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | no issues |
| Tests | `cargo test --locked --no-fail-fast` | exit 0; 112 result lines: 1472 passed, 0 failed, 3 ignored |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | no warnings |

The 3 ignored are the intentional ones: `device_smoke` and the two
`m8_snapshot_size` measurements. `main` at `5582b5c` ran 1468 passed; the 4
new tests are `a_report_becomes_the_notice_the_browser_shows`,
`a_mixed_batch_becomes_one_failed_notice`,
`local_operations_leave_the_http_slot_empty` (`application::feed_ops`) and
`describe_redacts_feed_and_station_urls` (`application::browse`).

## Layering

`tests/m9_layering.rs` passes with `ALLOWED` empty.

## CLI parity

`main` at `5582b5c` against the branch at `79edd78` (plus this PR's
documentation-only changes), both release builds. Each side had its own
`XDG_DATA_HOME`/`XDG_CACHE_HOME`, copied before each scenario from one
template seeded by the `main` binary. One local `python3 -m http.server`
served `rss2-minimal.xml` and `atom-minimal.xml` to both. stdout, stderr
(with `tracing`'s timestamp prefix stripped) and exit codes were compared
byte for byte with `diff -r`: **no difference**.

| Scenario | Commands | Result (identical on both sides) |
|---|---|---|
| 1 success | `subscribe <url> --as alpha`, `refresh alpha`, `unsubscribe alpha` | `alpha: subscribed, 1 episodes retained, 0 skipped`, `alpha: unchanged`, `alpha: unsubscribed`; exit 0 ×3; empty stderr |
| 2a early failure | `refresh no-such-slug` | no stdout; `tenuto: unknown feed: no-such-slug` and the log line `command failed error=Feed(UnknownSlug { slug: "no-such-slug" })`; exit 1 |
| 2b early failure, stdout closed | `refresh no-such-slug >&-` | the same unknown-slug error and log line; exit 1. This does not exercise a stdout failure: Rust's `Stdout` treats `EBADF` as a successful write, so `>&-` never fails a write on either binary. Stdout-over-operation precedence is pinned by the `write_report` unit test (`an_unwritable_stream_fails_the_command`) |
| 3 partial failure | `refresh` with `beta`'s URL returning 404 | `alpha: unchanged`, `beta: failed: the server answered HTTP 404 while Open`; `tenuto: 1 of 2 feeds did not complete successfully` and `Feed(BatchIncomplete { failed: 1, total: 2 })`; exit 1 |

`--as` coverage in the existing suites: `tests/m4_cli.rs` runs `subscribe
<url> --as radio-t` as a subprocess (lines 412, 818, 944, 948) and then
works under that slug.

Station operations have no CLI command; they are covered by the moved
`feed_ops` unit tests and `tests/m7_1_station_probe.rs`.
