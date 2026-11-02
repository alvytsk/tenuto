# Milestone 1 — known debt

Findings raised during M1 review that were deliberately deferred rather than
fixed. None blocks correctness of the shipped slice; each was judged against the
milestone's binding invariant (stop and transport recreation preserve the logical
playback position) and found not to threaten it. Milestone 2's carried debt is
recorded in the second half of this file.

## Error modelling

- **`PlaybackError::Failed(String)` is stringly-typed**, which the project's error
  policy forbids. The stringiness originates upstream: `Worker::fail(message: String)`
  and `PlaybackEvent::Failed { message: String }` already collapse every worker fault
  to a display string, so the CLI has no typed cause left to preserve. Fixing it
  honestly means giving the event protocol a typed cause.
- **`PlaybackError::Io` and `Failed` live in `src/playback/error.rs`** although the
  M1 plan's file list for that task did not include it. Recorded as a plan amendment.

## Decoder

- `seek_refined` reports `refinement_truncated: false` when refinement hits end of
  media, so the flag alone cannot distinguish "landed exactly" from "ran out". The
  engine compensates at the call site by comparing `actual` against the target.
- After `PlaybackError::Cancelled`, the source sits at an arbitrary mid-refinement
  position. No documented post-cancel contract; callers must re-establish position.
- `duration_to_frames` floors via `as u64`, so a sub-frame target lands one frame early.
- `self.pending = !self.planes[0].is_empty()` is a dead guard — the overshoot is
  always at least one frame inside that branch.

## Resampling

- `Converter::finish` carries an unexplained `MAX_FLUSH_CHUNKS = 8` cap. If
  `output_delay()` ever exceeded it, `finish` would return short with no error.

## Output and device handling

- `Phase::from_u8` / `Adopted::from_u8` map an unrecognised tag to `Run`/`Running` —
  fail-open rather than fail-safe. Unreachable today: only `as_u8` output is ever
  packed and a `u64` atomic cannot tear.
- The fault-classification fallback absorbs any variant cpal adds in future releases.
  Unavoidable given `#[non_exhaustive]`; a new variant should be classified explicitly.
- `NegotiatedOutput` fabricates `buffer_frames: 1024` while opening with
  `BufferSize::Default`. (Sample format is now negotiated: if the device's default
  config is not `f32`, its supported configurations are searched for one that is,
  preferring the default's sample rate, before refusing.)
- Integer-only devices remain unsupported. The engine writes `f32` buffers throughout;
  the negotiation guard only makes the refusal legible instead of an obscure failure
  when the stream is built. Supporting them means a format conversion at the callback.
- `OutputFault::Rebuild(DeviceBusy)` is the only Rebuild classification without a test.

## Engine

- Two owners call `link.take_diagnostics()` — `Worker::collect_diagnostics` and
  `Handshake::drain_spans`. No double-count (it is a swap), but xruns consumed inside
  `freeze_and_capture` never reach the aggregated warning, which therefore under-reports.
- A `Failed` event can carry a newer `session_rev` than the keep-latest `Progress`
  snapshot published a tick earlier. The app's `session_rev` guard correctly skips the
  stale snapshot; costs at most one render tick.
- After a discard timeout during a seek, `seek_to` still emits `SeekCompleted { actual }`
  even though `rebuild` has restored the pre-seek position, so the event and the position
  disagree. The position is correct, so the invariant holds, but the event carries the
  post-rebuild `session_rev` and cannot be filtered as stale. Fix before anything treats
  `actual` as authoritative (a checkpoint writer, a UI seek bar).
- A panic on the decode thread leaves the worker dead while the UI keeps rendering its
  last state: no `Failed` is emitted and the app appears frozen rather than reporting the
  fault. A dying worker should surface as `Failed`.
- `capture_and_teardown`'s rescue fallback reports frames *submitted* rather than *heard*,
  so a device loss taken while the callback holds an unpublished span reports a position up
  to one output latency ahead. Bounded by one output buffer and flagged `Degraded`.
- The playback path opens and probes the file twice: once to validate before entering raw
  mode, once in the worker. Eliminating it means threading an opened source through `Load`.
- Dead API: `EngineHandle::spawn`, `CpalOutput::drain_faults`, `CallbackCore::sample_rate`,
  `TestOutput::buffer_frames`.

## CLI

- `--help` and `--version` exit non-zero to stderr, because every `try_parse` error is
  treated as a failure. The bare-invocation behaviour is deliberate and tested; the
  explicit-flag case likely is not.

## Untested paths

- The fault **deferral** gate (holding a fault back when the event backlog has no room)
  has no end-to-end test. Command admission closes as soon as one event is pending, so a
  harness cannot build the ~121-deep backlog deferral needs by sending commands; reaching
  it requires many asynchronous events. The *retirement* rule that pairs with it is unit
  tested (`retire_faults`), and the gate arithmetic is documented beside the constant, but
  neither is evidence the gate fires correctly in a running engine.
- F1's whole-frame ring atomicity is structural (one commit per batch) and has no test:
  the race needs a consumer observing a partially written batch, which the sequential
  harness cannot stage.
- Integer-only and multichannel-only device refusals are unverified without such hardware.

## Not measured

- Span-ring capacity (64) and the PCM ring target (250 ms) remain the spec's starting
  values. Validating them needs a full track on real hardware with `spans_dropped` and
  xrun counts logged — part of the outstanding manual acceptance.

# Milestone 2 — carried debt

Findings from the M2 review that were judged fine to carry. None threatens the
milestone's invariant (a position the listener reached is never overwritten by
one the engine never validated); each is here so the next milestone inherits the
reasoning rather than rediscovering the finding.

## Writer thread

- The writer wakes ten times a second even with an empty slot, because
  `IDLE_WAIT` caps every wait in `src/persistence/writer.rs`. The principled fix
  is to read `closing` inside the slot's critical section, so an empty slot can
  wait unbounded and still be woken by the shutdown.
- A detach after the D10 timeout can leave an orphan
  `state.json.tmp-<pid>-<seq>` behind if the process dies mid-write
  (`src/persistence/store.rs`). It partly self-heals: a later run that reuses the
  pid hits `create_new` → `AlreadyExists`, removes the stale temp, and succeeds
  on the next sequence.
- `submits_after_shutdown_are_rejected` in `tests/persistence_writer.rs` passes
  for the wrong reason — by the time `shutdown()` has returned the writer thread
  has already exited, so the test would pass without the `closing` check it
  claims to pin.

## Untested paths

- D11's three-failure warning, D3's disable reasons and the honest-flush log are
  all policy expressed through `tracing` (`src/persistence/writer.rs`,
  `src/app.rs`), and none of them is assertable. A small
  `tracing_subscriber::Layer` collecting `(level, message)` pairs would close the
  whole class at once.
- D19's backlog half is covered only by a race
  (`an_event_racing_the_shutdown_interrupt_still_arrives` in
  `tests/engine_shutdown.rs`), which passes whichever side of the race wins. The
  deterministic companion it wants would overflow the worker past
  `EVENT_CAPACITY - RESERVED_EVENT_SLOTS`, so that `flush_events` provably breaks,
  and then assert that the overflow events arrive after the channel's.
- The atomic-replace test in `tests/persistence_store.rs` proves that two
  successive writes leave no stale tail, but it cannot simulate a crash. Crash
  safety rests on the mechanism being inspectable — `create_new`, `fsync`,
  `rename` — rather than on a test.

## Session policy

- `record_outgoing`'s "call this before the per-media fields reset" contract
  (`src/session.rs`) is documentary. Taking the three fields it reads as
  parameters would make the ordering a fact the borrow checker enforces.
- `playback` still describes the previous media for one iteration after a
  `Loaded` (`src/session.rs`), which is safe only because the establishment gate
  covers that window. The invariant it rests on — `playback == Playing` implies
  `established` — is asserted nowhere.

# Milestone 3 — carried debt

Findings from the M3 review that were judged fine to carry. None threatens the
milestone's invariant (HTTP does not by itself make media finite, seekable,
resumable, or live, and a resume point already reached is never quietly
overwritten by a fallback run); each is here so the next milestone inherits the
reasoning rather than rediscovering the finding. §12's acceptance coverage map
lives separately, in `docs/m3-acceptance.md`, since it is a test-to-requirement
table rather than a design judgement.

## M4A over HTTP

- **Resolved, not carried as debt: M4A opens over ranges and is refused
  sequentially, both verified.** The design's fallback text anticipated this
  being untested if ffmpeg were unavailable during acceptance work; ffmpeg 9.0.1
  was available, so `tests/fixtures/sine-5s.m4a` (a genuine tail-`moov`
  ISO-BMFF/AAC file, box layout confirmed by inspection, not accidentally
  `faststart`) exercises both directions in
  `tests/http_playback.rs::an_m4a_recording_opens_over_ranges`: it opens over a
  range-capable server and fails to open over a range-less one, because a
  tail-`moov` file needs byte seeking to open at all.

## Error modelling

- **`PlaybackEvent::Failed` still carries `message: String` beside its typed
  `cause: Option<RemoteFailure>`.** M3 narrows M1's "stringly-typed failures"
  finding rather than closing it: every remote fault now has a typed cause a
  policy can match on, but `cause` is `None` for every local (non-remote)
  fault, so those still carry only a string a human wrote. Closing this
  fully would mean typing M1's local decoder/device faults too, which is
  outside M3's scope.
- **`RemoteFailure::InvalidSource.input` is a plain `String`**, with nothing in
  the type system enforcing that it was redacted before being stored there.
  Every construction site does redact (`redact_url` is called at each one,
  verified by reading them), so the risk is latent rather than live, but a
  newtype (`RedactedUrl` or similar) would make "this string cannot contain a
  secret" a fact the compiler checks instead of a fact every call site has to
  remember.
- **`RemoteFailure::Display` interpolates `Debug` output into prose**
  (`"the server answered HTTP {status} while {operation:?}"` reads as `"...
  while Open"`, `"redirect refused: {reason:?}"` reads as `"... :
  UnsupportedScheme"`). Functionally fine — nothing downstream parses these
  strings — but it reads awkwardly wherever a human sees it directly (a status
  line, a bare log line).
- **`Transport.detail` (`src/http/error.rs`) carries only reqwest's one-line
  kind string**, with no source chain and no URL. The design's "preserve
  operation and source context" would be better served by appending the
  underlying error's `source()` chain rather than only its top-level
  `to_string()`; the chain still would not carry a URL, since none of these
  detail strings do (consistent with keeping signed query strings and userinfo
  out of diagnostics).
- **Two dead error variants, and `operation` context that is half-preserved.**
  `Operation::Reopen` and `Operation::Complete` are constructed nowhere in this
  codebase — every `FetchRequest` this project ever builds carries
  `Operation::Open` or `Operation::Seek`, never the other two. `Phase::Connect`
  is likewise constructed nowhere; reqwest's own connect timeout surfaces
  through `Transport`, not `Timeout { phase: Connect }`. Relatedly,
  `response::accept` (`src/http/response.rs`) does not use the `operation` a
  `FetchRequest` actually carried at all — it derives `Operation::Open` versus
  `Operation::Seek` for `RemoteFailure::Status` from `at_origin` instead, so
  §11's "preserve operation context" is only half-delivered: the fact is
  reconstructed from a different signal rather than carried through. Removing
  the two dead variants, or rethreading `operation` through `accept` so it is
  the one source of truth, is a design change this wave does not make.

## Capability evidence

- **H17's first half is undischarged, and the reason is narrow.**
  `symphonia-bundle-mp3`'s demuxer returns `SeekErrorKind::Unseekable` for a
  track with no `num_frames` — exactly the case H17 wants — but only on the
  `SeekMode::Coarse` branch (`preseek_coarse`), and `src/playback/decode.rs:290`
  pins `seek_refined` to `SeekMode::Accurate`, whose `preseek_accurate` never
  consults `num_frames` and just scans forward. So `DemuxerSeek::Unproven` →
  `SeekSupport::Unknown` and the engine's matching refusal
  (`verify_seek_support` rejecting a seek it could not demonstrate) are
  **defensive rather than reachable**, given that one line. A future switch to
  `Coarse` — for the trial seek specifically, not necessarily for ordinary
  seeking — makes them reachable and makes the row testable. FLAC, WAV, and
  `symphonia-format-isomp4` (checked for the M4A/ALAC case) were also read and
  none has an analogous reachable refusal.

  **Update, from a since-investigated bug report:** the pin to
  `SeekMode::Accurate` is not merely a testing-reachability limitation, as the
  paragraph above implies — it is a live defect for HTTP sources.
  `preseek_accurate` rewinds to `first_packet_pos` and forward-rescans
  whenever `required_ts < self.next_packet_ts`; the trigger has nothing to do
  with whether an Xing/VBRI index exists (the `Accurate` branch never reads
  `num_frames`), and the rewind is conditional, not automatic on every seek.
  `next_packet_ts` is the demuxer's own read-ahead position, which runs ahead
  of audible playback by the PCM ring plus `MediaSourceStream`'s own
  read-ahead, so a seek that is forward in audible terms can still be
  backward relative to `next_packet_ts` and fire the rewind. The scan that
  follows parses frame headers without decoding them — cheap locally,
  expensive over HTTP, since the bytes still have to arrive in order through
  the byte channel. The whole rewind-and-rescan runs inside one
  uncancellable, unbounded call to `FormatReader::seek`; `SEEK_BUDGET`
  (`engine.rs:116`) cannot bound it, because that budget only gates
  `seek_refined`'s residual-alignment loop *after* `reader.seek()` already
  returns, never the scan itself.

  **This paragraph's claim that `SeekMode::Coarse` is not a drop-in escape
  because `preseek_coarse` "refuses outright... whenever `num_frames` is
  absent" is false, and is corrected here.** `preseek_coarse`'s `Unseekable`
  refusal (`symphonia-bundle-mp3-0.6.1/src/demuxer.rs:503-513`) fires only
  when the stream itself is not byte-seekable, or when `max_ts` cannot be
  computed at all. For a byte-seekable file with no Xing/Info/VBRI tag,
  `MpaReader::try_new` (`demuxer.rs:465-471`) still calls
  `estimate_num_mpeg_frames` and populates `track.num_frames` from a
  ~16-frame sample, so `max_ts` is available and `preseek_coarse` proceeds —
  it does not refuse. Verified both by reading the source and by exercising
  it (`docs/superpowers/specs/2026-09-10-tenuto-estimated-seek-design.md`
  §5.2).

  **Fixed by M3.1 Task 4.** `seek_refined` (`src/playback/decode.rs`) now
  requests `SeekMode::Coarse` rather than `Accurate` for every seek —
  including `verify_seek_support`'s own trial seek, which routes through the
  same call — computing a byte offset directly from the track's duration
  arithmetic instead of rewinding and rescanning. Measured against the file
  that reproduced this defect: `Coarse` consumed 3,072 bytes against the
  983,040 bytes the `Accurate` rescan it replaces consumed for the same
  seek. The reproduction, renamed to
  `tests/engine_remote.rs::a_short_forward_seek_on_a_no_index_mp3_lands_quickly_without_rescanning`,
  is un-`#[ignore]`d and is now the regression test. `Coarse`'s own landing
  accuracy on a no-Xing-tag VBR file is a separate, still-open concern —
  recorded under Milestone 3.1 below, not this entry, which was about the
  rescan hang and is closed.
- **`verify_seek_support`'s two possible outcomes are asymmetrically tested.**
  `tests/engine_remote.rs::a_capability_change_carries_the_current_session_rev`
  proves the `Unknown → Native` transition (a stopped seek's trial succeeds and
  publishes `CapabilitiesChanged`). Its sibling, `Unknown → Unsupported`, has no
  test — and, tied to the H17 finding above, the code does not currently
  perform that transition either: on a failed trial, `verify_seek_support`
  (`src/playback/engine.rs:2522`) returns `false` and the caller rejects the
  seek, but `self.capabilities.seek` is left at `Unknown` rather than being
  advanced to `Unsupported`, so a later seek attempt re-runs the same trial
  rather than remembering the negative result. Harmless today because no
  reachable fixture ever makes the trial fail (the same reason H17's first half
  is undischarged), but worth fixing alongside whatever addresses H17, since a
  reachable failure would otherwise cost a repeated network round trip on every
  subsequent stopped seek.
- **The domain-reset regression test is mechanism-level.**
  `src/playback/engine.rs`'s own unit test
  `a_domain_reset_uses_a_plain_store_so_fetch_max_does_not_latch_the_old_high_water_mark`
  replays the `store`/`fetch_max`
  sequence on a local `AtomicU64`; it would still pass if `open_transport`'s
  plain `store` were reverted to `fetch_max` in the real code, which is the
  exact regression its neighbouring comment warns against — the test proves the
  *composition* is correct in isolation, not that the real call site still uses
  it. A real test needs a multi-domain output harness (`TestOutput`'s clock
  never goes backward, so it cannot stage the old domain's high-water mark
  against a fresh one). The placement was verified instead by tracing every
  path into `open_transport`.
- **No regression test for a buffer-satisfied short seek** — one where
  Symphonia satisfies the seek from `MediaSourceStream`'s own read-ahead
  without ever reaching `HttpMediaSource::seek`. Structurally unreachable
  today because `SeekSupport::Unknown` is only produced when the transport is
  byte-seekable and the demuxer is unproven, and the engine's seek gate refuses
  every source that would reach a playing-state seek without a real
  `MediaSource::seek()` call to answer it first — but that reasoning is what
  protects the invariant `open_transport`'s comment relies on, not a test.
- **"Range-capable *and* live" is untestable through the current test
  server.** `TestServer`'s `write_206` (`tests/support/server.rs`) structurally
  cannot carry `icy-*` headers alongside a 206 response, so every live-source
  test in this suite must also be range-less. The combination is real (a
  range-capable server can still be an ICY live stream) and the refusal logic
  does not special-case it away, but no fixture in this project can currently
  demonstrate it end-to-end.

## HTTP transport

- **The HTTP/2 stream window is set from `chunk_bytes` (64 KiB) rather than
  `buffer_bytes`** (`src/http/service.rs`), more conservative than a
  single-stream connection needs — the connection window already carries the
  1 MiB `buffer_bytes` cap, and a stream window narrower than the connection
  window can force extra `WINDOW_UPDATE` round-trips under fast transfer. It
  violates no documented bound (the total the design promises is unaffected),
  just spends more protocol chatter than the loosest correct setting would.
- **The `SLICE` constant in `src/http/channel.rs`** (20 ms) sets how often a
  blocked reader's wait re-tests its predicate and the wait hook runs — and
  therefore how current a checkpoint and published progress stay during a
  network stall. It is sized only against the design's one-second wake bound
  (§8: "source waits wake within one second after stop, seek or shutdown"),
  not measured against real network stall behaviour on a real connection.
- **`Limits::buffer_bytes` is not actually injectable** (`src/http/limits.rs`),
  despite the struct's own doc comment and §8's "injectable for tests". Every
  `SourceInterrupt::new` call site in production code (`EngineHandle::
  assemble`, `src/playback/engine.rs`) reads `Limits::default().buffer_bytes`
  directly rather than a caller-supplied `Limits` — the interrupt's buffer is
  sized once, for the worker's whole life, before any per-session `Limits`
  exists to read. Its only real consumer today is the HTTP/2 connection
  window (`HttpService::spawn`), which does read the injected value. Fixing
  the wiring — threading a per-load `Limits` into the one buffer that has to
  outlive every source a worker ever opens — is a design change, not a fix
  this wave makes; the field's doc comment now says what is actually true.
- **`resolve_url` (`src/app.rs`) parses its input twice** — once directly with
  `Url::parse` (to check for embedded credentials before identity is ever
  built) and once again inside `NormalizedUrl::parse` (identity
  normalization). This is an intentional consequence of keeping identity and
  the fetch URL as separate types with separate parsing rules, not an
  oversight; a shared internal parse would couple the two in a way the design
  deliberately avoids.

## Plan deviations (§13)

Changes made during implementation, recorded per the design's requirement that
such changes be explicit rather than silent:

- **`run_thawed` was built and then removed**, in favor of deleting the
  fetch-loop freeze gate it existed to work around. **`wait_while_frozen` was
  removed entirely** for the same reason — pausing is the output's job, not a
  reason to stop a network read from making progress; gating the read only
  starved work that legitimately still needed to happen while paused.
- **`SourceEvidence` and `DemuxerSeek` live in `src/media/capabilities.rs`**,
  not in the `http` module — both a local file and a remote one construct
  them (`DecodedSource::open` builds local evidence with
  `DemuxerSeek::Proven`), so an HTTP-flavoured location would make local
  playback construct a network type to describe itself.
- **`FetchAccepted` lives in `src/http/response.rs`**, beside the other
  response-shaped types it is built from, rather than beside the fetch task
  that produces it.
- **The header handoff lives in `src/http/channel.rs`**, on the same lock and
  condvar as the byte buffer and the interrupt state, rather than beside the
  fetch task in `src/http/service.rs`. Every flag change and the header result
  need the same lost-wake-proof synchronization the buffer already has, so
  giving them their own separate lock would reintroduce the class of race the
  shared lock exists to close.
- **`OpeningDeadline` and `OpeningLimits` live in `src/http/source.rs`**,
  where `HttpMediaSource::open` and every wait it takes during opening can
  reach them directly, rather than in the service module that constructs the
  service-wide `Limits`.
- **Raw mode is entered before the worker loop and made optional, rather than
  deferred.** The original plan's R5 described raw mode as entered only after
  the first `Loaded`/`Failed`; what actually needed deferring was
  *rendering*, not raw-mode entry. `RawModeGuard::enable` (`src/app.rs`)
  returns `None` when there is no controlling terminal (`enable_raw_mode`
  fails for want of a tty — the CI case), and a session that starts with no
  tty reads no keys but still prints `Loading` and any failure normally.

# Milestone 3.1 — carried debt

Findings from executing the estimated-seeking amendment
(`docs/superpowers/specs/2026-09-10-tenuto-estimated-seek-design.md`) that
were judged fine to carry. None threatens the amendment's invariant (an
estimated position or duration may drive display and resume, but must never
replace an established checkpoint or drive a destructive decision). All four
were established during the implementation spike rather than planned for, so
the plan could not name them in advance.

- **An MP3 `Coarse` landing has no useful accuracy bound.** Measured error
  grows from 0.4 s at a 10 s target to **235 s of error on a 600 s file** as
  the target approaches the file's estimated duration ceiling — at a 360 s
  target it lands at 595 s while self-reporting a plausible ~360 s, with no
  signal to the caller that anything is wrong. The error is bounded only by
  the file's own length, which is not a guarantee a listener asking to land
  at a specific point can use. This is why every MP3 landing reports
  `PositionProvenance::Estimated`, unconditionally. The conjunction that
  produces it: no Xing/Info/VBRI tag, so `num_frames` comes from
  `estimate_num_mpeg_frames`'s ~16-frame sample rather than an encoder's own
  count, **and** genuinely variable bitrate, so even a correct frame count
  would not make uniform-bitrate byte arithmetic exact. A file with a tag —
  which includes the podcast from the manual acceptance report (`Info` tag,
  exact frame count, CBR to within 0.03 s over 2 h 16 m) — lands essentially
  exactly, because the denominator driving the estimate is then the
  encoder's own declared count rather than a 16-frame extrapolation.
- **The tail of an under-estimated file is unreachable, deliberately.**
  Symphonia's own `max_ts` check (`symphonia-bundle-mp3-0.6.1/src/
  demuxer.rs:268-272`) refuses a target past the estimated ceiling *before*
  the mode dispatch at `:292-296`, so the refusal is mode-independent by
  construction and no choice of `SeekMode` recovers it. `clamp_target`
  (`src/playback/engine.rs:2615`) no longer clamps to an *estimated*
  duration — only to an established one, via `established_duration` — which
  turned a silent mislanding at the estimated ceiling into a visible,
  honest `OutOfRange` refusal; it did not extend how far into such a file a
  seek can actually reach. Retained, not a bug to fix here.
- **Shapes A (re-probe at the byte offset) and B (byte-seek beneath a live
  reader), rejected by the Task 1 spike.** Shape A is implementable but needs
  either a codec-specific constructor called directly or a `MediaSource`
  adapter, because `Probe::probe()` cannot probe from an offset:
  `MediaSourceStream::new` always initialises its internal `abs_pos` to 0
  with no way to tell it the source starts elsewhere, and `Probe::probe()`'s
  trailing-metadata pre-scan restores to `mss.pos()` — 0, by the above —
  before the main scan runs. Corrected and measured, shape A costs one
  request at the estimated byte and needs the same 3-frame reservoir discard
  `Coarse` also needs, but carries the identical duration-estimate
  vulnerability as the finding above plus a bespoke adapter and manual
  reservoir/timestamp-origin bookkeeping that `Coarse` gets for free from
  `MpaReader::seek`'s existing machinery. Shape B is **structurally
  unimplementable**: `FormatReader`'s only route back to the underlying
  stream is `into_inner(self: Box<Self>)`, which consumes the reader, and
  nothing exposes `next_packet_ts` for external mutation even if a live
  handle existed — reaching the concrete `MpaReader` would mean downcasting
  the trait object, which this crate's `unsafe_code = "forbid"` and ordinary
  API discipline both rule out.
- **A better duration estimate was not attempted.**
  `estimate_num_mpeg_frames` samples only the first ~16 frames
  (`demuxer.rs:684`, `MAX_FRAMES: u32 = 16`) and extrapolates. Sampling frame
  headers at ten points across the file would cost roughly 40 KB against the
  983 KB rescan this amendment removes, and would collapse the dominant
  error term in the first finding above. Symphonia's own Xing TOC — parsed
  into `XingInfoTag.toc` (`demuxer.rs:749-757`, `#[allow(dead_code)]`) and
  never read — would do better still if it were reachable through the
  public API. Neither is attempted: this amendment removes a wedge, and
  widening it into duration-estimator work was deliberately kept out of
  scope.

# Milestone 7 — carried debt

Findings from the M7 live-radio work that were judged fine to carry. Neither
threatens the milestone's invariant (recovery continues a current user
request and never seeks listening time); each is a stated limit of what M7
plays, not a defect in what it plays.

- **A Shoutcast v1 station (a bare `ICY 200 OK` status line, no HTTP version
  token) is unplayable.** hyper parses that line as malformed and reqwest
  surfaces it as a transport error, so the initial open fails at once and a
  reconnect exhausts its budget against the same error on every attempt. The
  status line never reaches `response::accept`, so no amount of ICY-header
  handling on Tenuto's side can classify it.
- **A stream with no ICY headers at all stays `Continuity::Unresolved` and is
  refused,** the same as before M7 — `response::is_live` has nothing to key
  on. The upgrade path is a station library entry with an explicit `--live`
  override, so a listener who knows a URL is a station can say so instead of
  waiting on header sniffing; out of scope for M7 (spec §11).
