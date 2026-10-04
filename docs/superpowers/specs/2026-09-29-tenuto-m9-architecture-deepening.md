# Tenuto M9: architecture deepening

Status: roadmap, recorded on 2026-09-29 from an architecture review of `main` at `67942e8`. It names the nearest architecture improvements and orders them by layer. It is not a design: each sub-milestone gets its own spec and plan before any code changes, and an item may be dropped when its spec is written.

## 1. Goal

Turn the shallow modules that M5–M8 left behind into deep ones: a small interface with the behavior behind it, tested through that interface. The review found the same rules written in several modules, a few rules enforced only by comments, and tests that must reach past an interface or wait on wall time.

M9 changes no user-visible behavior except the defects named below. `state.json` keeps schema 4. The position contract in `architecture.md` §1 and the decisions in §12 stand: no backend trait, no repository trait, position lives in the worker.

Vocabulary: a **module** has an **interface** (everything a caller must know) and an implementation. It is **deep** when a lot of behavior sits behind a small interface, **shallow** when the interface is about as complex as the implementation. A **seam** is where an interface lives.

## 2. Layers and order

Work goes bottom-up through the layers of `architecture.md` §4. M9.1 comes first because the navigator in M9.2 builds on it. The other sub-milestones are independent of each other and can be taken in any order.

| Sub-milestone | Layer | Items |
|---|---|---|
| M9.1 | Foundation: durable state | PlaylistSet · versioned JSON file · resume intent |
| M9.2 | Application | adopted-playback mirror · enricher · cover tracker · playlist navigator · lookup by `MediaId` |
| M9.3 | Entry | feed operations · `play` over `PlayerRuntime` |
| M9.4 | Playback engine | submission door · landing · transport · event outbox |
| M9.5 | Sources and tests | network deadlines on `Clock` · runtime rig on the virtual clock |

## 3. M9.1 — Foundation: durable state

**PlaylistSet owns the playlist rules.** Start here.
- Problem: the M8 rules P1, P2, P3 and P7, the shuffle pin and the "next, else previous" successor are each enforced in two to four places: `persistence/model.rs:254-401`, `persistence/queue_codec.rs:244-540`, `session.rs:457-515, 633-652, 1131-1147`, `application/runtime.rs:1225-1265`. `PersistedState::playlist_mut` and `queue_mut` hand out `&mut Queue`, so only "only `Session`" comments keep callers inside the rules. Tests enter through `queue_mut_for_tests`, `from_parts_for_tests` and `from_raw_for_tests`.
- Direction: one module owns the playlists, `playing`, both ID allocators and the cap. No `&mut Queue` escapes it. Codec recovery feeds its constructor. `PersistedState` becomes the envelope around it, and `Session` keeps load correlation and ownership.
- Tests after: the rules are tested directly on the new module, without `Session` fixtures. `m8_session_playlists` shrinks to ownership and invalidation.

**One versioned JSON file module.**
- Problem: `persistence/store.rs`, `subscription/store.rs` and `station/store.rs` each carry the same lifecycle: not found, unreadable, unsupported version, quarantine with up to 100 candidates, and `stamp()`. The subscription and station stores differ only in nouns. `m4_subscription_store` tests that lifecycle, and `m7_1_station_store` does not. `library::load_mutating` and `load_mutating_stations` are copies of each other.
- Direction: one concrete module with a read-only snapshot, a load-for-mutation that refuses anything not writable, and a validated save. It takes a file name, a schema version and a decode function. Keep it a function or a generic helper, not a trait (§12).
- Tests after: one lifecycle suite on a tempdir. The per-store suites keep only record validation.

**Resume intent lives in `resume.rs`.**
- Problem: `resume_intent_for` sits in `session.rs`. Both callers repeat the `None` → `StartAt(ZERO)` default (`session.rs:1613`, `app.rs:545`). `restart_preference` was tested for a milestone before it had a production caller.
- Direction: move the function, return a plain `ResumeIntent`, make its helpers private.

## 4. M9.2 — Application

**One adopted-playback mirror.**
- Problem: `app.rs` (`Mirror` :714, `apply_progress` :339) and `application/runtime.rs` (`Mirror` :246) keep two copies of the display mirror, progress gating and the optimistic `Estimated` seek, and the copies have drifted. Opening persistence is also written twice (`app.rs:578-652`, `tui/mod.rs:256-305`).
- Defect: `app.rs`'s `SeekCompleted`, `EndOfTrack` and `RestartEstablished` arms ignore provenance.
- Defect: `tenuto play` loads through `LoadTarget::Legacy`, which clears the playing playlist's cursor (`session.rs:1144`).
- Direction: one mirror module and one "open profile state" function in `application`, used by both front ends.

**Enricher owns tag policy.**
- Problem: `MetadataWorkers` is shallow, and the runtime holds the policy: `request_enrichment` :1298, `apply_enrichment` :1328, the re-offer in `vacate` :1245. The #29 fix had to keep `RESULT_CAPACITY` in step with `MAX_ENRICHMENT_PER_PUMP` across two files. Tests reach this only through the wall-clock runtime rig.
- Direction: the enricher is offered state and entry IDs, and yields a ready batch of display updates. It owns deduplication, generations, per-playlist cancellation and the batch size. Tag probing sits behind an injectable probe.

**Cover tracker.**
- Problem: choosing the active cover is split between three runtime queries (`cover_key` :485, `active_cover` :509, `active_cover_kind` :553) and a deduplication private to the TUI (`Artwork::poll`, `tui/mod.rs:503-560`). `tests/m7_1_station_artwork.rs:330` records that the real mechanism cannot be driven from a test.
- Direction: an application-side tracker yields the active entry's cover kind and image state. The TUI keeps only terminal encoding and placement.

**Playlist navigator.** Depends on PlaylistSet.
- Problem: the viewed playlist, its selection and the browser's destination rows are split between the runtime and the TUI. The browser gets its rows two ways: `view.rows` at `tui/mod.rs:401`, and a fresh `Arc` at `tui/mod.rs:780`. The fresh `Arc` defeats the `Arc::ptr_eq` skip that #29 added.
- Direction: one navigator holds view, selection, hints and destination rows, and reconciles itself when the state's edit count moves. It uses PlaylistSet's successor rule.

**Lookup by `MediaId` in `library`.** Speculative.
- Problem: `application/podcast.rs` repeats the "find the feed, read the cache, find the episode" walk that `library` already does by slug, and runs it on the main thread.
- Direction: add lookup by `MediaId` for episodes and stations to `library`. `podcast.rs` then goes away.

## 5. M9.3 — Entry

**Feed operations below `commands.rs`.**
- Problem: `commands.rs::run` and `application/browse.rs::mutate` dispatch the same seven arms. The application and TUI import upward from the entry layer: `wait_http`, `report`, `finish_*`, `displayable`, the stores. "A partial success never exits zero" is encoded as an `Ok` carrying a `followup`, which every caller must know means failure.
- Direction: a feed-operations module beside `library` returns report text plus a complete-or-incomplete classification. `commands.rs` keeps column layout and exit-code mapping, and `displayable` moves down a layer. The §4 component table needs revising, because `commands` stops deciding completeness.

**`play` over `PlayerRuntime`.** Worth exploring after the M9.2 mirror work.
- Direction: `tenuto play` becomes a one-entry front end over the runtime, and `LoadTarget::Legacy` retires.

## 6. M9.4 — Playback engine

**One submission door on `EngineHandle`.**
- Problem: correctness depends on the caller choosing among `submit`, `submit_seek`, `submit_pause` and `submit_play`, plus raw `commands()` and `wake()`, all at `engine.rs:333-490`. A plain `submit(SeekTo)` is accepted, but it skips the generation retirement the #13 fix relies on. `application/seek.rs::route_command` decides the `TogglePause` direction on the caller's side, and `KeyRouter::observe` hard-codes the six events that settle a seek.
- Direction: `submit(command)` applies every out-of-band rule itself, and the raw senders become private. `PlaybackEvent` gains a `settles_seek` predicate beside `is_terminal`. This strengthens §12: commands and events stay the application's seam.

**Landing.**
- Problem: the sequence seek → adopt position and provenance → prove capability → reinstall → classify the outcome is written separately in `load`, `restore`, `seek_to`, `restart` and `rebuild`, and the copies have drifted. `seek_to` computes truncation, `restore` hard-codes `false`, and `is_cancelled` missed `Remote(Cancelled)` until a later fix round.
- Direction: one operation lands the decoder at a target under a policy and returns a closed outcome. The worker maps that outcome to state and events. Position still lives in the worker (§12).
- Tests after: a fixture source that retires or times out at a chosen byte. No device and no clock.

**Transport owns park.** Worth exploring.
- Problem: pause has two writers, `Worker::pause` and the hook's freeze. They coordinate through `SessionFacts.frozen_by_hook` and `reconcile_frozen_by_hook` (`engine.rs:2940`). Reading a position crosses the worker, `WaitService`, `TransportCore`, `Handshake` and `Timeline`.
- Direction: one transport module owns the ring, link, handshake, timeline, generation, anchor and park state, and park is idempotent. `AudioOutput` stays the device seam, so no backend trait is needed (§12).

**Event outbox.** Speculative.
- Problem: the worker and `wait.rs` both emit events, and they keep ordering through a mirrored `backlog_empty` flag.
- Direction: one outbox owns the bounded sender, the pending queue and the reserve policy.

## 7. M9.5 — Sources and tests

**Network deadlines on `Clock`.**
- Problem: several clocks are wall time, while the device runs on the virtual clock:
  - the opening budget (`http/source.rs:127, 154`);
  - the stall budget (`http/channel.rs:385-470`);
  - the operation deadline (`engine.rs:3644`);
  - reconnect outage timing (`engine.rs:2155, 2211`).

  CI flakes #15, #16 and #18 came from these two time sources interacting. (#13 was a send-then-retire generation race, fixed in e51bae3; a clock would not have helped.) `tests/m7_reconnect.rs:340, 379` sleeps on wall time. The open protocol (`OpeningDeadline` → `set_probe_cap` → `finish_opening`) is replayed in `playback/prepare.rs` and `library.rs`.
- Direction: fold the deadline, probe cap and failure latch into one open-and-probe operation. Inject the existing `clock::Clock` into the HTTP source and into reconnect scheduling. `SystemClock` and `FakeClock` already exist as its two adapters. Risk: `ByteChannel`'s waits must wake when a fake clock advances.
- Status: the clock is injected (`SourceInterrupt::with_clock`, `EngineHandle::spawn_on_clock`). Waits re-read it every 20 ms slice rather than being woken, and stall demand is charged across the whole pass so a step taken in the wait hook is not lost. `TestEngine::start_on_fake_clock` holds it; `m7_reconnect` and `m10_finite_reconnect` use it in place of wide real-time backoffs and sleeps. The open-and-probe fold is still open.

**Runtime rig on the virtual clock.**
- Problem: `tests/support/runtime.rs` builds the runtime over `NullOutput` with `SystemClock`, and `pump_until` and `pump_for` sleep 10 ms per pass.
- Direction: the rig's `EngineFactory` builds the virtual-clock `TestOutput` engine instead. Do not fake `EngineHandle` behind a trait (§12).

## 8. Out of scope

- New features.
- Schema changes.
- A backend or repository trait.
- Moving position out of the worker.
- The M7.2 ICY metadata work, which keeps its own spec.
