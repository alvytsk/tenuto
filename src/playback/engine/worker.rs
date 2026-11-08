//! The decode worker and its command state machine.

//! The decode worker: the state machine that owns every pipeline component.
//!
//! The invariant this module exists to enforce is that stopping or recreating
//! the audio pipeline never implicitly resets the logical playback position.
//! Position lives here, in `Worker::position`, and only four things establish a
//! new one: loading media, an explicit restart, a successful seek, and a resume
//! that adopts a target stored while stopped. Everything else — pausing,
//! stopping, a device disappearing, a handshake timing out — preserves it.

use std::any::Any;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Sender, select};
use url::Url;

use crate::http::channel::{ByteChannel, ReadOutcome, SourceInterrupt, WaitHook};
use crate::http::error::{Operation, RemoteFailure};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::{is_retired, remote_cause};
use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;
use crate::media::source::SourceLocation;
use crate::resume::{KnownDuration, ResumeDecision, decide_resume};
use crate::telemetry::redact_url;

use super::super::callback::CallbackCore;
use super::super::command::{LoadRequestId, PlaybackCommand, ResumeIntent};
use super::super::decode::{DecodedSource, SeekOutcome};
use super::super::error::PlaybackError;
use super::super::event::{PlaybackEvent, Progress, StartDisposition};
use super::super::handshake::Handshake;
use super::super::link::OutputLink;
use super::super::lock;
use super::super::outbox::Outbox;
use super::super::output::cpal_output::OutputFault;
use super::super::output::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest, SpanRecord};
use super::super::prepare::{PrepareContext, prepare};
use super::super::reconnect::{Outage, ReconnectPolicy, retryable};
use super::super::resample::Converter;
use super::super::spectrum::worker::SpectrumPort;
use super::super::state::PlaybackState;
use super::super::timeline::Timeline;
use super::super::wait::{SessionFacts, WaitService};
use super::diagnostics::GlitchCounters;
use crate::media::provenance::PositionProvenance;
use crate::volume::Volume;

use super::handle::SourceTraits;
use super::{
    DEADLINE, PUMP_NAP, RESUME_TOLERANCE, RING_MILLIS, SEEK_BUDGET, SHUTDOWN, SPAN_CAPACITY, STOP,
    TICK, TransportCore, TransportTap,
};

/// How [`Worker::land`] treats the decoder's answer (M9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Approach {
    /// The listener's own seek: refinement may give up after `SEEK_BUDGET`,
    /// and the position is what the decoder reports.
    Seek,
    /// A position the session already promised (a resume, a recovery, a
    /// restart): refined to the end, and a sub-frame difference keeps the
    /// promised value (`adopt_preserved`).
    Preserve,
}

/// Where a landing put the decoder.
#[derive(Clone, Copy, Debug)]
pub(super) struct Landing {
    actual: Duration,
    /// Refinement gave up early, or the decoder stopped short of the target.
    truncated: bool,
}

/// [`Worker::land`]'s closed outcome. A caller maps each arm to state and
/// events; none of them has to re-derive which errors are cancellations.
#[derive(Debug)]
pub(super) enum Landed {
    At(Landing),
    /// A stop, a shutdown or a newer seek retired the landing. The decoder
    /// is at an arbitrary point, and the interrupt still stands for the loop.
    Cancelled,
    Failed(PlaybackError),
}

/// M10 §4: what a resume must establish, held until a landing is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingResume {
    Seek(Duration),
    Restart,
}

impl PendingResume {
    pub(super) fn target(self) -> Duration {
        match self {
            Self::Seek(target) => target,
            Self::Restart => Duration::ZERO,
        }
    }
}

pub(super) struct Worker {
    pub(super) output: Box<dyn AudioOutput>,
    pub(super) faults: Receiver<OutputFault>,
    pub(super) transport: Arc<Mutex<Option<TransportCore>>>,
    /// The PCM producer side of the ring. `!Sync`, so it stays here rather
    /// than behind `transport`'s lock — see `TransportCore`'s doc comment.
    pub(super) pcm: Option<rtrb::Producer<f32>>,
    pub(super) link: Option<Arc<OutputLink>>,
    pub(super) config: Option<NegotiatedOutput>,
    source: Option<DecodedSource>,
    /// Where `source` was opened from, set by `load` and cleared by
    /// `shutdown`. Survives a remote source's retirement - unlike `source`
    /// itself, which `retire_remote_source` drops - so `ensure_source_open`
    /// has something to reopen from and `restore`/`play` know a stopped or
    /// failed session is a remote one worth reopening rather than a dead end.
    descriptor: Option<SourceLocation>,
    /// What `load` and `ensure_source_open` last established about `source`.
    /// Read by the seek gates in `restore` and `seek_to` so they answer from
    /// one field rather than reaching into a source that may currently be
    /// `None` (a retired remote source between a stop and its reopen).
    pub(super) capabilities: MediaCapabilities,
    pub(super) converter: Option<Converter>,
    /// Interleaved output samples the ring has not accepted yet.
    pub(super) staging: Vec<f32>,
    pub(super) state: PlaybackState,
    pub(super) session_rev: u64,
    /// The logical resume point. Stop and recreation preserve it; load,
    /// restart and successful seeks establish a new one.
    pub(super) position: Duration,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3, M3.1). Sticky: ordinary playback never changes it, and a seek
    /// that performs no actual `seek_refined` call (`land`'s no-op, or a
    /// load that never moves off zero) leaves it exactly as it was, never
    /// resetting it to `Established` by default. `publish_progress` mirrors
    /// this into `facts.provenance` on every pass, the same way it mirrors
    /// `position` and `degraded`.
    pub(super) position_provenance: PositionProvenance,
    /// A seek or restart stored but not yet established (M10 §4). Read, never
    /// taken, by every resume; cleared only once a landing is installed.
    pub(super) pending: Option<PendingResume>,
    /// The established duration of a finite source in recovery, cached on
    /// entry because the decoder that knew it is gone (M10 §3). Clamps a seek
    /// stored offline. Cleared by `load`.
    recovery_duration: Option<Duration>,
    pub(super) media: Option<MediaId>,
    /// The load currently in flight, set by `load` before `StateChanged`
    /// announces `Loading` and taken (cleared) the moment that load reaches
    /// an outcome: `Loaded` takes it into `adopted_load`, and both
    /// cancellation returns plus `fail_with` clear it directly (M5 §6).
    loading: Option<LoadRequestId>,
    /// The load whose media the worker currently holds, set when `Loaded` is
    /// emitted. Stop, pause, seek and device recovery never touch it; only
    /// the next `load` clears it, before announcing its own `Loading`.
    pub(super) adopted_load: Option<LoadRequestId>,
    volume: Volume,
    /// The transport generation: bumped for every new source, seek, device
    /// rebuild. Worker-side this is u32 — the comparisons it feeds
    /// (`heard_mark`, the rescue-record match) live as long as the worker,
    /// and 65,536 bumps — a held key-repeat's worth of seeks — would alias
    /// a u16 onto a mark recorded a lifetime ago. The RT wire keeps u16
    /// (`Handshake`, `Timeline`, `OutputLink`'s packed word, the spectrum
    /// tap): an in-flight span or ack survives at most a handful of
    /// generations, so the low 16 bits cannot alias there. Cast at the
    /// boundary, never widened back for a comparison.
    pub(super) generation: u32,
    pub(super) pushed_total: u64,
    pub(super) source_eof: bool,
    pub(super) converter_flushed: bool,
    pub(super) decoder_drained: bool,
    pub(super) degraded: bool,
    /// Whether an attempt (a station's `fresh_open`, a finite `prime_attempt`)
    /// is priming a source it has not committed to yet (M7 §5.2, M10 §5).
    /// While set, a read failure is recorded in `attempt_failure` instead of
    /// transitioning the session.
    attempting: bool,
    attempt_failure: Option<PlaybackError>,
    shutting_down: bool,
    pub(super) receivers_gone: bool,
    pub(super) commands: Receiver<PlaybackCommand>,
    /// The engine's one event queue and its delivery rules, shared with the
    /// wait hook (`playback::outbox`).
    pub(super) outbox: Arc<Outbox>,
    liveness: Sender<()>,
    wake: Receiver<()>,
    interrupt: Arc<AtomicU8>,
    pub(super) glitches: GlitchCounters,
    /// A collapsed fault held back because the event backlog had no room for
    /// the events acting on it would produce.
    pub(super) deferred_fault: Option<OutputFault>,
    pub(super) device_warnings: u64,
    /// The scalars the shared `service` needs, mirrored from the fields above
    /// on every `publish_progress` pass. Lock order: this before `transport`
    /// (Ruling 1) — enforced by `WaitService`, which is the only thing that
    /// ever takes both.
    pub(super) facts: Arc<Mutex<SessionFacts>>,
    /// Shared with the wait hook wired to every open remote source: the one
    /// implementation `publish_progress` and `WaitHook::service` both call
    /// (Ruling 4).
    pub(super) service: Arc<WaitService>,
    /// The one `SourceInterrupt` this worker's whole life uses (Carried
    /// Finding 2). Handed to `prepare` as `PrepareContext::interrupt` for
    /// every source this worker ever opens, and to `WaitService` at
    /// construction — the same instance throughout, never swapped.
    pub(super) source_interrupt: Arc<SourceInterrupt>,
    /// The handle's out-of-band view of `capabilities`, written only by
    /// `set_capabilities` (M7 §6.1).
    pub(super) traits: Arc<SourceTraits>,
    /// Shared with `EngineHandle::set_http`, so a service installed after
    /// this worker was spawned is visible the next time `load` builds a
    /// `PrepareContext`.
    http: Arc<Mutex<Option<Arc<HttpService>>>>,
    /// Shared with `EngineHandle::set_reconnect_policy`. Read fresh at every
    /// decision point, never cached, so an installed policy takes effect at
    /// once (M7 §7).
    pub(super) reconnect_policy: Arc<Mutex<ReconnectPolicy>>,
    /// The outage in progress: `Some` from the first retryable failure of a
    /// playing connection, live or finite (M10), until sustained playback
    /// ends it, a failure ends the session (every one of them, through
    /// `fail_with`), or the listener ends the request (pause - a park
    /// included - stop, a replacing load, shutdown). `None` at every other
    /// moment - which is what lets each outage be judged against a budget of
    /// its own.
    pub(super) outage: Option<Outage>,
    /// Where heard-time accounting last read the current generation (M10
    /// §6): its number and how far past its anchor the position was.
    heard_mark: Option<(u32, Duration)>,
    /// A `Send + Sync` mirror of the device's current instant, so
    /// `WaitService` — reachable from inside a decoder read that already
    /// holds `&mut self.source` and so cannot see the rest of `Worker`, let
    /// alone `output` — has a clock to read the transport with. Two writers:
    /// `CallbackCore::fill`, on the audio backend thread, on every
    /// invocation — including while parked or frozen — which is the one
    /// context still running while the decode thread is blocked, and
    /// `Worker::publish_progress`, immediately before it calls into
    /// `service`, which is what keeps an ordinary loop pass exactly as fresh
    /// as `self.output.now()` rather than up to one buffer period stale. See
    /// `publish_progress`'s, `CallbackCore::fill`'s and `WaitService`'s own
    /// `clock` field's doc comments.
    pub(super) device_clock: Arc<AtomicU64>,
    /// Builds each transport's spectrum tap and holds the registry its
    /// mappings are published to.
    spectrum: SpectrumPort,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        progress: Arc<Mutex<Progress>>,
        commands: Receiver<PlaybackCommand>,
        outbox: Arc<Outbox>,
        liveness: Sender<()>,
        wake: Receiver<()>,
        interrupt: Arc<AtomicU8>,
        source_interrupt: Arc<SourceInterrupt>,
        traits: Arc<SourceTraits>,
        http: Arc<Mutex<Option<Arc<HttpService>>>>,
        reconnect_policy: Arc<Mutex<ReconnectPolicy>>,
        device_clock: Arc<AtomicU64>,
        spectrum: SpectrumPort,
    ) -> Self {
        let transport = Arc::new(Mutex::new(None));
        let facts = Arc::new(Mutex::new(SessionFacts {
            session_rev: 0,
            media: None,
            position: Duration::ZERO,
            degraded: false,
            playing: false,
            provenance: PositionProvenance::Established,
            frozen_by_hook: false,
            load: None,
        }));
        let clock: Arc<dyn Fn() -> Nanos + Send + Sync> = {
            let device_clock = Arc::clone(&device_clock);
            Arc::new(move || Nanos(device_clock.load(Ordering::Relaxed)))
        };
        // The exact same `Arc` `EngineHandle::assemble` constructed, sized for
        // real fetch throughput from the start (Carried Finding 2) - never a
        // placeholder swapped out later, since `WaitService` and every source
        // this worker ever opens must agree on one instance.
        let service = WaitService::new(
            Arc::clone(&transport),
            Arc::clone(&progress),
            Arc::clone(&facts),
            Arc::clone(&source_interrupt),
            Arc::clone(&outbox),
            clock,
        );
        Self {
            output,
            faults,
            transport,
            pcm: None,
            link: None,
            config: None,
            source: None,
            descriptor: None,
            capabilities: MediaCapabilities {
                continuity: Continuity::Unresolved,
                seek: SeekSupport::Unknown,
            },
            converter: None,
            staging: Vec::new(),
            state: PlaybackState::Idle,
            session_rev: 0,
            position: Duration::ZERO,
            position_provenance: PositionProvenance::Established,
            pending: None,
            recovery_duration: None,
            media: None,
            loading: None,
            adopted_load: None,
            volume: Volume::FULL,
            generation: 0,
            pushed_total: 0,
            source_eof: false,
            converter_flushed: false,
            decoder_drained: false,
            degraded: false,
            attempting: false,
            attempt_failure: None,
            shutting_down: false,
            receivers_gone: false,
            commands,
            outbox,
            liveness,
            wake,
            interrupt,
            glitches: GlitchCounters::start(Instant::now()),
            deferred_fault: None,
            device_warnings: 0,
            facts,
            service,
            source_interrupt,
            traits,
            http,
            reconnect_policy,
            outage: None,
            heard_mark: None,
            device_clock,
            spectrum,
        }
    }

    pub(super) fn run(mut self) -> Vec<PlaybackEvent> {
        let commands = self.commands.clone();
        let wake = self.wake.clone();
        let liveness = self.liveness.clone();
        loop {
            // 1. Out-of-band interrupts. Shutdown dominates stop.
            let flags = self.interrupt.swap(0, Ordering::Acquire);
            if flags & SHUTDOWN != 0 {
                self.shutdown();
                return self.outbox.take_remaining();
            }
            if flags & STOP != 0 {
                self.do_stop();
            }
            // An accepted seek carries no worker-side action. Its retirement
            // woke the read, and the queued `SeekTo` carries the target; the
            // generation the seek runs under is opened by `begin()` at the
            // point of use, not here — arming here would run after
            // `do_stop` and undo the retirement it just published. The `swap`
            // above already consumed the bit; there is nothing further to do
            // with it here.

            // 2. Spans -> timeline -> keep-latest progress snapshot, through
            //    the same `WaitService` the hook uses (one implementation,
            //    two callers - Ruling 4).
            self.collect_diagnostics();
            self.publish_progress();

            // 3. Asynchronous device faults.
            self.service_faults();

            // 4. Flush the event backlog. Admission stays closed while it is
            //    non-empty, which bounds further command-driven generation.
            self.flush_events();

            // 5. Diagnostics coalesce; they never enter the outbox's queue.
            self.emit_aggregated_warning_if_slot_free();

            // 6. Wait. Never a bare sleep - stop and shutdown must wake it.
            let admit = self.outbox.is_empty();
            let mut received: Option<Result<PlaybackCommand, RecvError>> = None;
            let mut disconnected = false;
            if admit {
                select! {
                    recv(commands) -> command => received = Some(command),
                    recv(wake) -> _ => {}
                    send(liveness, ()) -> result => disconnected = result.is_err(),
                    default(TICK) => {}
                }
            } else {
                select! {
                    recv(wake) -> _ => {}
                    send(liveness, ()) -> result => disconnected = result.is_err(),
                    default(TICK) => {}
                }
            }
            self.receivers_gone |= disconnected;
            match received {
                Some(Ok(command)) => self.dispatch(command),
                // Nobody can drive this engine any more.
                Some(Err(_)) => {
                    self.shutdown();
                    return self.outbox.take_remaining();
                }
                None => {}
            }
            if self.shutting_down {
                self.shutdown();
                return self.outbox.take_remaining();
            }

            // 7. Reconnect timing first - it is what turns `Reconnecting`
            //    back into `Playing` - then decode -> convert -> ring, with
            //    interruptible backpressure.
            self.service_reconnect();
            if self.state == PlaybackState::Playing {
                self.pump_audio();
            }
            self.check_end_of_track();

            // A disconnected event receiver is shutdown. Detected from the
            // channel's own errors - a disconnect the outbox latched when
            // flushing or offering events, and `RecvError` above when the
            // command sender is gone - never from channel occupancy, which
            // says nothing about connectedness.
            if self.receivers_gone {
                self.shutdown();
                return self.outbox.take_remaining();
            }
        }
    }

    // ----------------------------------------------------------------- events

    /// Emits `LoadCancelled` for the load still in flight, if any — the one
    /// outcome a `Load` abandoned mid-open owes its caller (M5 §6). Called
    /// from both cancellation returns in `load`, and from `shutdown`'s drain
    /// of undelivered `Load` commands.
    fn cancel_load(&mut self) {
        if let Some(request) = self.loading.take() {
            let session_rev = self.session_rev;
            self.emit(PlaybackEvent::LoadCancelled {
                session_rev,
                request,
            });
        }
    }

    fn reject_seek(&mut self, reason: String) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::SeekRejected {
            session_rev,
            reason,
        });
    }

    /// Announce playback only when there is a transport to play it.
    ///
    /// Recovery can abandon a rebuild without failing - a stop arriving while
    /// it re-seeks, say - and the caller must not report `Playing` over a
    /// pipeline that no longer exists.
    ///
    /// `lock(&self.transport)` here is a temporary: it is dropped at the end
    /// of the `if` condition, before the body runs, so `set_state` below is
    /// never called while the guard is still held. This pattern — a
    /// condition-position `lock(...)` whose guard cannot outlive the
    /// condition — recurs throughout this file (`reinstall`, `restore`,
    /// `restart`, `play`'s match guard) and is always this same rule, not
    /// repeated at every site.
    pub(super) fn announce_playing(&mut self) {
        if lock(&self.transport).is_some() {
            self.set_state(PlaybackState::Playing);
        }
    }

    /// The one writer of the worker's state and of `pausable`; of
    /// `recovering`, the one clearing writer - `enter_reconnecting`
    /// raises it ahead of the `Reconnecting` adoption, so a racing
    /// `submit_pause` retires rather than freezes. `pausable` is what
    /// `TogglePause` resolves against, so state and
    /// flag are written together or they drift: a `Paused` state behind a
    /// `true` pausable turned every later Space into a pause-submit the
    /// `Playing` guard swallowed, with no event and no resume (H1, project
    /// review 2026-11-01).
    ///
    /// Returns whether the state changed. The announcement is `set_state`'s
    /// half, kept separate for the two hook-landing pause writes: the wait
    /// hook announced `Paused` itself when it parked the transport inside a
    /// blocked read, and that announcement is allowed to stand alone.
    pub(super) fn adopt_state(&mut self, state: PlaybackState) -> bool {
        // M10 §7: every exit from Reconnecting ends a finite recovery.
        if state != PlaybackState::Reconnecting {
            self.traits.recovering.store(false, Ordering::Release);
        }
        if self.state == state {
            return false;
        }
        self.state = state;
        self.traits.pausable.store(
            matches!(state, PlaybackState::Playing | PlaybackState::Reconnecting),
            Ordering::Release,
        );
        true
    }

    pub(super) fn set_state(&mut self, state: PlaybackState) {
        if !self.adopt_state(state) {
            return;
        }
        let session_rev = self.session_rev;
        // Only a `Loading` announcement carries a token: it is the one state
        // change a load itself causes, so `play()`'s remote-reopen `Loading`
        // - reached with `self.loading` already `None` because `Loaded`
        // already took it - correctly carries `None` too, with no special
        // casing here (M5 §6, Decision 2).
        let request = if state == PlaybackState::Loading {
            self.loading
        } else {
            None
        };
        self.emit(PlaybackEvent::StateChanged {
            session_rev,
            state,
            request,
        });
    }

    pub(super) fn fail(&mut self, message: String) {
        self.fail_with(message, None);
    }

    /// `fail`'s general form. A remote failure carries a typed `cause`
    /// alongside `message`, so a policy can act on what went wrong rather
    /// than only read a string a human wrote; M1's local failures have none,
    /// which is why `fail` is still the entry point every local call site
    /// uses (Ruling 1).
    pub(super) fn fail_with(&mut self, message: String, cause: Option<RemoteFailure>) {
        if self.state == PlaybackState::Failed {
            // A repeating fatal fault must not re-announce the same failure
            // every iteration.
            return;
        }
        // M7 §7, and the single funnel every failure in this file passes
        // through (`fail` and `fail_from` both delegate here): a failed
        // session keeps no outage. Left standing, one would outlive the
        // session that opened it and be charged against the *next* one - and
        // since `Outage::failed` judges the budget from `started` before
        // anything else, a `started` minutes old makes the next disconnect
        // give up instantly instead of reconnecting. §9's one explicit reopen
        // out of `Failed` is exactly the path that would inherit it.
        //
        // After the guard above rather than before it: nothing can open an
        // outage on an already-`Failed` session (`source_ended` runs only from
        // `pump_audio`, which runs only while `Playing`, and
        // `service_reconnect` acts only in `Playing`/`Reconnecting`), so the
        // first `fail_with` has already cleared it and a repeat has nothing
        // left to do. The guard stays a pure early return, which is what its
        // own comment says it is.
        self.outage = None;
        self.teardown();
        // §9: the decoder over a remote source is unusable once its fetch is
        // dead - its `MediaSourceStream` sits over a channel nothing will
        // ever feed again - so it is dropped here, while `descriptor`,
        // `media`, `capabilities` and `position` all survive for the one
        // explicit reopen `play()`'s `Failed` arm permits. A local failure
        // keeps its decoder exactly as M1 always has; `retire_remote_source`
        // is a no-op for one. The interrupt is retired too, on the same
        // "every remote failure path retires" rule `do_stop` follows -
        // harmless when nothing was in flight, and correct when something
        // was.
        self.source_interrupt.retire();
        self.retire_remote_source();
        let session_rev = self.session_rev;
        // `take`, not a read: only a failure while a load was still in
        // flight is that load's outcome (M5 §6). Taking it here means a
        // later fault on the same session - after `Loaded` already took
        // `loading` for itself - correctly reports no request at all.
        let request = self.loading.take();
        self.emit(PlaybackEvent::Failed {
            session_rev,
            message,
            cause,
            request,
        });
        self.set_state(PlaybackState::Failed);
    }

    /// `fail`'s general form, from a `PlaybackError` rather than an
    /// already-split message and cause. `PlaybackError::Remote` is unwrapped
    /// directly, since `RemoteFailure` is not itself `#[source]`-annotated
    /// on that variant's transparent `Display`/`source()` forwarding and so
    /// `remote_cause` cannot recover it by walking the chain; every other
    /// variant is walked with `remote_cause`, which is how a decode error
    /// Symphonia mangled on the way up (Ruling 4 finding, `prepare.rs`'s own
    /// `promote_latched`) still reports the typed cause underneath it.
    pub(super) fn fail_from(&mut self, error: PlaybackError) {
        match error {
            PlaybackError::Remote(failure) => {
                let message = failure.to_string();
                self.fail_with(message, Some(failure));
            }
            other => {
                let cause = remote_cause(&other);
                self.fail_with(format!("{other}"), cause);
            }
        }
    }

    // --------------------------------------------------------------- progress

    // ----------------------------------------------------------------- faults

    /// Room a fault must leave behind before it is acted on.
    ///
    /// Two for the fault's own pair (`Failed` + `StateChanged`, or
    /// `DeviceRecovered` + `StateChanged`), plus the terminal work that can
    /// still follow it: end of track is another two, and a load in the same
    /// pass is four (`StateChanged{Loading}`, `Loaded`, `CapabilitiesChanged`,
    /// `StateChanged{Paused}`) now that a load whose initial seek proves a
    /// remote source's previously-unknown demuxer support emits
    /// `CapabilitiesChanged` on top of `Loaded` (the outbox's reserve-budget
    /// comment already carries this same four). Reserving only its own pair
    /// let a fault land at 126 pending, leaving no room for the EOF that
    /// followed - which then evicted a lifecycle event.
    pub(super) const FAULT_EVENT_BUDGET: usize = 8;

    /// Capture, tear down, then rebuild at the preserved position.
    ///
    /// `resume` is the state the caller *intends*, not the state the worker
    /// happens to be in: a recovery reached from `resume` or `restart` runs
    /// before either has announced `Playing`, and reading it off `self.state`
    /// would bring the transport back parked while the caller announced that
    /// playback had started.
    ///
    /// Three outcomes, and callers must tell them apart:
    ///
    /// * `Ok(())` — recovered; the transport is live again.
    /// * `Err(Cancelled)` — a stop or shutdown arrived during the recovery
    ///   re-seek. Nothing failed and nothing was emitted: the interrupt is
    ///   still set, and the loop's next pass turns it into the transition the
    ///   user actually asked for. A caller that reported this as a failure
    ///   would turn a requested stop into a spurious `Failed`, which `do_stop`
    ///   would then refuse to correct.
    /// * any other `Err` — recovery failed, `Failed` has already been emitted
    ///   with the position preserved, and the caller must announce nothing.
    pub(super) fn rebuild(&mut self, reason: &str, resume: bool) -> Result<(), PlaybackError> {
        if self.source.is_none() {
            self.teardown();
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no media is loaded".into(),
            });
        }
        self.capture_and_teardown();
        // M7 §5.2: a station's decoder is exactly where the listener is, and
        // the position it kept is listening time rather than a byte offset.
        // Re-seeking it would fail on a source that cannot seek at all, and
        // turn a device fault into a lost session.
        if !self.is_indefinite() {
            match self.land(self.position, Approach::Preserve) {
                Landed::At(_) => {}
                Landed::Cancelled => return Err(PlaybackError::Cancelled),
                Landed::Failed(error) => {
                    self.fail(format!("cannot recover after {reason}: {error}"));
                    return Err(error);
                }
            }
        }
        self.session_rev += 1;
        match self.open_transport(resume) {
            Ok(()) => {
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::DeviceRecovered { session_rev });
                Ok(())
            }
            Err(error) => {
                self.fail(format!("cannot reopen the audio device: {error}"));
                Err(error)
            }
        }
    }

    // -------------------------------------------------------------- transport

    fn open_transport(&mut self, playing: bool) -> Result<(), PlaybackError> {
        let (source_rate, source_channels) = match self.source.as_ref() {
            Some(source) => (source.sample_rate(), source.channels()),
            None => {
                return Err(PlaybackError::UnsupportedInput {
                    path: Default::default(),
                    reason: "no media is loaded".into(),
                });
            }
        };
        let config = self.output.negotiate(&OutputRequest {
            preferred_rate: source_rate,
            preferred_channels: source_channels,
        })?;
        let channels = config.channels.max(1);
        // M1 is scoped to mono and stereo, and `Converter` only knows how to
        // fan a source out to one or two device channels. A 6- or 8-channel
        // default is routine on HDMI and PipeWire, and the interleave fallback
        // would quietly emit two samples per frame into a buffer the engine
        // strides at six: 3x-fast playback and a 3x-inflated position, with no
        // error anywhere. Refuse the device instead. Downmixing is a later
        // milestone.
        if !matches!(channels, 1 | 2) {
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: format!(
                    "the audio device negotiated {channels} channels; \
                     only mono and stereo output is supported"
                ),
            });
        }
        let link = Arc::new(OutputLink::new());
        link.set_gain(self.volume.as_gain());

        let ring_frames = (u64::from(config.sample_rate) * RING_MILLIS / 1_000)
            .max(u64::from(config.buffer_frames) * 4) as usize;
        // A whole number of frames, so the ring can never come to hold a
        // partial frame: the callback pops sample by sample and would emit one
        // channel of a frame while its siblings stayed silent.
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(ring_frames * usize::from(channels));
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(SPAN_CAPACITY);
        let core = CallbackCore::new(
            Arc::clone(&link),
            pcm_rx,
            span_tx,
            channels,
            config.sample_rate,
            // The callback thread is the one context still running while the
            // decode thread is blocked inside a read, so it is the only
            // place that can keep `device_clock` live for `WaitService`
            // during that block (see `CallbackCore::fill`'s doc comment).
            Arc::clone(&self.device_clock),
        );
        // A fresh instance per transport, its reader already with the
        // analysis worker before the callback can write a block.
        let (tap_instance, tap_writer) = self.spectrum.open_tap(channels, config.sample_rate);
        let core = core.with_tap(tap_writer);
        // Plain `store`, deliberately, not `fetch_max`: everywhere else in
        // this file `fetch_max` is correct precisely because writer and
        // reader stay inside one clock domain, but opening a stream *changes*
        // the domain — a rebuilt `cpal::Stream` restarts its own
        // `StreamInstant` near zero, and `CpalOutput::now()` returns
        // `Nanos(0)` for the interval this call is about to close. A
        // `fetch_max` here would latch the OLD domain's high-water mark
        // forever, since every value the NEW domain ever produces is lower
        // than it — freezing the reported position after every device
        // rebuild, which is exactly the M1 recovery path the contract tests
        // exist to protect. Do not "simplify" this to `fetch_max` to match
        // the two steady-state writers above; it is not the same problem.
        self.device_clock.store(0, Ordering::Relaxed);
        self.output.open(&config, Arc::clone(&link), core)?;

        let mut timeline = Timeline::new(config.sample_rate);
        self.converter = Some(Converter::new(
            source_rate,
            config.sample_rate,
            source_channels,
            channels,
        )?);
        let mut handshake = Handshake::new(Arc::clone(&link), span_rx);
        self.reset_generation_state();
        // The wire keeps the low 16 bits of the generation (see the field's
        // doc); in-flight protocol state cannot live long enough to alias.
        let generation = self.generation as u16;
        // The anchor this generation counts from: captured now, before the
        // new `TransportCore` exists to hold it, and handed straight to its
        // constructor below.
        let anchor = self.position;
        let installed = {
            let mut pump = || std::thread::sleep(PUMP_NAP);
            handshake.install(generation, false, &mut timeline, &mut pump, DEADLINE)
        };
        if installed.is_err() {
            self.output.close();
            return Err(PlaybackError::Timeout);
        }
        self.link = Some(link);
        self.pcm = Some(pcm_tx);
        let sample_rate = config.sample_rate;
        self.config = Some(config);
        let tap = TransportTap {
            registry: self.spectrum.registry().clone(),
            instance: tap_instance,
            session_rev: self.session_rev,
            channels,
        };
        *lock(&self.transport) =
            Some(TransportCore::new(handshake, timeline, anchor, sample_rate).with_tap(tap));
        self.prime_and_run(playing);
        Ok(())
    }

    /// M7 §5.2. One sequence for reconnect attempts and for every Play on an
    /// established station. Never seeks; never reports a seek or a restart.
    ///
    /// `Ok` means `Playing` was announced. On `Err` nothing of the new source
    /// remains adopted and no transport this call opened is left behind.
    ///
    /// What survives an `Err` is narrower than it looks. Step 0 closes the
    /// old source before anything else, so by the time a refusal in step 1
    /// returns, the connection is already gone; what is still standing is the
    /// previous generation's *transport*, holding the frames already staged
    /// in its ring, which go on playing out while the caller decides what the
    /// failure means. A failure from step 3 onward has torn even that down.
    pub(super) fn fresh_open(&mut self) -> Result<(), PlaybackError> {
        let Some(location) = self.descriptor.clone() else {
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no media is loaded".into(),
            });
        };
        // 0. Whatever is still open goes first. This session shares ONE
        //    `SourceInterrupt`, and dropping an `HttpMediaSource` retires it
        //    (its `Drop`) — so a decoder still held here would be dropped by
        //    step 4's assignment and kill the generation step 1 had just
        //    begun. Nothing is lost by closing early: this sequence exists to
        //    replace whatever is open, and every caller either has no source
        //    left (a pause, a stop, a failure) or is explicitly rejoining the
        //    live edge.
        self.source_interrupt.retire();
        self.retire_remote_source();
        // 1. Prepared, not adopted. A changed continuity is refused in here,
        //    before this session's position or state has been given up.
        let mut context = self.prepare_context();
        context.expected = Some(Continuity::Indefinite);
        let prepared = prepare(&location, &context)?;
        // 2.
        if self.source_interrupt.is_retired() || self.interrupted() {
            return Err(PlaybackError::Cancelled);
        }
        // 3. The old generation's final played position becomes the anchor;
        //    its ring is discarded with it.
        self.capture_and_teardown();
        // 4. Capabilities are unchanged by construction: nothing to announce.
        self.source = Some(prepared.source);
        // 5 + 6. `open_transport(false)` builds conversion for THIS decoder
        //    and primes once. While `attempting`, a source that dies during
        //    priming reports through `attempt_failure` instead of
        //    transitioning the session.
        self.attempting = true;
        self.attempt_failure = None;
        let opened = self.open_transport(false);
        self.attempting = false;
        // 7, hoisted ahead of every other classification: a stop or a
        // shutdown retires the priming read, and `pump_audio` answers that
        // through its `is_retired_read` arm - which drops the source and
        // returns *quietly*, leaving no `attempt_failure` and a `primed` that
        // reads false precisely because the source is gone. Classified as
        // anything but a cancellation, that becomes a fabricated
        // `Failed{LiveEnded}`, and `do_stop`'s own early return on `Failed`
        // then swallows the stop the listener actually asked for - the same
        // rule `restore` and `restart` already state at their own cancellation
        // arms. This subsumes the check that used to sit after the
        // classification: nothing between here and `announce_playing` blocks,
        // so one check covers both points, and it also catches a cancelled
        // `opened` without having to guess which `PlaybackError` shape a
        // cancelled device open would take.
        if self.source_interrupt.is_retired() || self.interrupted() {
            self.abandon_attempt();
            return Err(PlaybackError::Cancelled);
        }
        let primed = self.source.is_some() && (self.pushed_total > 0 || !self.staging.is_empty());
        let failure = self.attempt_failure.take();
        if let Err(error) = opened {
            self.abandon_attempt();
            return Err(error);
        }
        if let Some(failure) = failure {
            self.abandon_attempt();
            return Err(failure);
        }
        if !primed {
            self.abandon_attempt();
            return Err(RemoteFailure::LiveEnded.into());
        }
        self.start_running();
        self.announce_playing();
        if let SourceLocation::Http(url) = &location {
            tracing::debug!(url = %redact_url(url.as_str()), "live source opened fresh");
        }
        Ok(())
    }

    /// Give up what `fresh_open` had opened but not yet announced, leaving no
    /// transport and no half-adopted decoder behind.
    pub(super) fn abandon_attempt(&mut self) {
        self.teardown();
        self.source_interrupt.retire();
        self.retire_remote_source();
    }

    /// `fail_with`, then commit only if nothing failed and nothing cancelled.
    /// `Ok` leaves the transport primed and parked, for the caller to start;
    /// `Err` leaves no transport and no decoder.
    ///
    /// No `primed` check, unlike `fresh_open`: a finite target at the very
    /// end primes nothing and is still a landing, which end-of-track follows.
    pub(super) fn prime_attempt(&mut self) -> Result<(), PlaybackError> {
        self.attempting = true;
        self.attempt_failure = None;
        let opened = self.reinstall(false);
        self.attempting = false;
        // First, as in `fresh_open`: a stop, shutdown, pause or seek retires
        // the priming read, and `pump_audio` answers that quietly. Remote
        // only: `do_stop` retires the shared interrupt and a local source
        // never begins a new generation, so a local Stop → Play would read
        // as cancelled forever.
        let retired = self.source_is_remote() && self.source_interrupt.is_retired();
        if retired || self.interrupted() {
            self.abandon_attempt();
            return Err(PlaybackError::Cancelled);
        }
        let failure = self.attempt_failure.take();
        if let Err(error) = opened {
            self.abandon_attempt();
            return Err(error);
        }
        if let Some(error) = failure {
            self.abandon_attempt();
            return Err(error);
        }
        Ok(())
    }

    /// Re-adopt the existing transport under a fresh generation: discard the
    /// ring the old generation filled, install, prime, and release.
    fn reinstall(&mut self, playing: bool) -> Result<(), PlaybackError> {
        if lock(&self.transport).is_none() {
            return self.open_transport(playing);
        }
        if let Some(converter) = self.converter.as_mut() {
            converter.reset();
        }
        // The discard comes first, and the generation state is reset only once
        // it has succeeded. Both are recoveries into `rebuild`, but they meet
        // the timeline in opposite conditions: `install` resets the timeline as
        // its first action, so a timeout there leaves `capture_position` with a
        // voided timeline and nothing to misread. A timeout in `discard` leaves
        // the *previous* generation's spans standing, and if the anchor had
        // already been moved to the freshly seeked target, the capture would
        // read back `anchor_new + played_old` - a silent forward jump of
        // everything played since the last install.
        let discarded = {
            let mut guard = lock(&self.transport);
            let Some(core) = guard.as_mut() else {
                return Ok(());
            };
            let mut pump = || std::thread::sleep(PUMP_NAP);
            core.handshake.discard(&mut pump, DEADLINE)
        };
        // The device stopped answering. Recreate it rather than run on against
        // a transport whose state can no longer be established - and pass the
        // outcome through unchanged, so the caller can tell a failed recovery
        // from a cancelled one and announce neither.
        if discarded.is_err() {
            return self.rebuild("handshake timeout", playing);
        }
        self.reset_generation_state();
        // The wire keeps the low 16 bits of the generation (see the field's
        // doc); in-flight protocol state cannot live long enough to alias.
        let generation = self.generation as u16;
        let anchor = self.position;
        let installed = {
            let mut guard = lock(&self.transport);
            let Some(core) = guard.as_mut() else {
                return Ok(());
            };
            // Set here, through the lock, rather than at construction: unlike
            // `open_transport`, this generation reuses an already-existing
            // `TransportCore` rather than building a new one.
            core.anchor = anchor;
            let TransportCore {
                handshake,
                timeline,
                ..
            } = core;
            let mut pump = || std::thread::sleep(PUMP_NAP);
            handshake.install(generation, false, timeline, &mut pump, DEADLINE)
        };
        if installed.is_err() {
            return self.rebuild("handshake timeout", playing);
        }
        self.prime_and_run(playing);
        Ok(())
    }

    /// Everything that a new generation invalidates, in one place.
    ///
    /// The anchor - `self.position` at the moment this generation starts
    /// counting from - is not among them: it lives in `TransportCore` now, so
    /// a caller building a brand-new one (`open_transport`) passes it straight
    /// to `TransportCore::new`, and `reinstall`, which reuses an existing one,
    /// writes it back through the lock itself, right after this returns.
    fn reset_generation_state(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pushed_total = 0;
        self.staging.clear();
        self.source_eof = false;
        self.converter_flushed = false;
        self.decoder_drained = false;
        self.degraded = false;
    }

    /// Fill the ring before releasing the callback, so that starting playback
    /// does not begin with a self-inflicted underrun.
    ///
    /// The ring is primed even when the transport stays parked: a seek taken
    /// while paused installs a new generation, and the `release` that resumes
    /// it would otherwise start against an empty ring.
    fn prime_and_run(&mut self, playing: bool) {
        self.pump_audio();
        if !playing {
            return;
        }
        self.start_running();
    }

    /// Release the current generation into `Run`. `prime_and_run`'s tail,
    /// extracted so `fresh_open` can prime first and only start the output
    /// once the new source has proven it produces audio (M7 §5.2).
    pub(super) fn start_running(&mut self) {
        let mut guard = lock(&self.transport);
        if let Some(core) = guard.as_mut() {
            let generation = core.handshake.generation();
            core.start_running(generation);
        }
    }

    /// Whether a stop or a shutdown is pending. Read rather than consumed:
    /// the loop's own `swap(0)` is what acts on it.
    pub(super) fn interrupted(&self) -> bool {
        stop_or_shutdown(self.interrupt.load(Ordering::Acquire))
    }

    /// M10 §6: credit the outage with what was heard since the last reading.
    /// A new generation (a seek, a reopen, a device rebuild) starts again from
    /// its own anchor, so a seek moves nothing here.
    pub(super) fn settle_heard(&mut self) {
        let Some(anchor) = lock(&self.transport).as_ref().map(|core| core.anchor) else {
            return;
        };
        let generation = self.generation;
        let offset = self.position.saturating_sub(anchor);
        let since = match self.heard_mark {
            Some((marked, last)) if marked == generation => offset.saturating_sub(last),
            _ => offset,
        };
        self.heard_mark = Some((generation, offset));
        if let Some(outage) = self.outage.as_mut() {
            outage.add_heard(since);
        }
    }

    fn shutdown(&mut self) {
        self.outage = None;
        // Accepted but never dispatched: each still owes its caller an
        // outcome. Every other undelivered command is simply discarded -
        // nothing else in `PlaybackCommand` promises one.
        while let Ok(command) = self.commands.try_recv() {
            if let PlaybackCommand::Load { request, .. } = command {
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::LoadCancelled {
                    session_rev,
                    request,
                });
            }
        }
        let captured_exactly = self.capture_position();
        self.teardown();
        self.source = None;
        // `descriptor`'s own doc comment says "set by
        // `load`, cleared by `shutdown`" - make that true rather than leave
        // it stale. Latent either way, since every `shutdown()` call site
        // returns immediately after, but a stale value contradicts the
        // comment describing it.
        self.descriptor = None;
        // Quiet, not `set_state`: nothing is left to announce to, but the
        // traits still clear through the one writer.
        self.adopt_state(PlaybackState::Idle);
        if !captured_exactly {
            // The published quality checks `degraded` before it looks at
            // `state`, so this is what keeps a capture that timed out from
            // being published as Exact on the strength of `state` having just
            // become `Idle`.
            self.degraded = true;
        }
        // With the transport gone the recompute branch is skipped, so this
        // publishes precisely the captured position (D14). Without it, the last
        // Progress a reader can see is the one from the previous pass and the
        // capture is unreachable.
        self.publish_progress();
    }

    // ------------------------------------------------------------------ audio

    /// Ruling 3: must not hold the transport lock across `source.next_planar()`
    /// below, or a remote read that blocks deadlocks the worker the moment it
    /// does. In fact it never touches the transport lock at all: `pcm` and
    /// `config` are plain `Worker` fields precisely so this loop - the one
    /// place a decoder read can block - has nothing here to hold across it.
    fn pump_audio(&mut self) {
        loop {
            let Some(config) = self.config.as_ref() else {
                return;
            };
            let channels = usize::from(config.channels.max(1));
            let Some(pcm) = self.pcm.as_mut() else {
                return;
            };
            // Whole frames only, in a ring sized as a multiple of the channel
            // count: a partial frame here would emit one channel of a frame
            // while its siblings stayed silent.
            let free = pcm.slots() / channels * channels;
            let ready = self.staging.len() / channels * channels;
            let count = free.min(ready);
            if count > 0 {
                // Publish the whole batch with ONE tail advance. Pushing sample
                // by sample makes each one visible immediately, so the callback
                // can pop a left channel whose right sibling has not been
                // written yet - a half frame, mid-push, even though `count` is a
                // whole number of frames.
                if pcm.push_entire_slice(&self.staging[..count]).is_ok() {
                    self.staging.drain(..count);
                    self.pushed_total += (count / channels) as u64;
                }
            }
            if !self.staging.is_empty() {
                // The ring is full; the loop's wait provides the backpressure.
                return;
            }
            if self.source_eof {
                if !self.converter_flushed {
                    self.converter_flushed = true;
                    if let Some(converter) = self.converter.as_mut() {
                        converter.finish(&mut self.staging);
                    }
                    continue;
                }
                self.decoder_drained = true;
                return;
            }
            let decoded = {
                let Self {
                    source,
                    converter,
                    staging,
                    ..
                } = self;
                let (Some(source), Some(converter)) = (source.as_mut(), converter.as_mut()) else {
                    // A retired remote source is
                    // `None` here with `descriptor` still naming it - the
                    // very next pass after `is_retired_read`'s own arm ran,
                    // since an accepted seek takes no step-1 action and
                    // `select!` can
                    // pick `recv(wake)` over the queued `SeekTo` on any given
                    // pass. That is not EOF; it is nothing to decode *yet*,
                    // and must not be converted into one by falling through
                    // to `decoder_drained` on the next iteration. Only a
                    // genuinely absent source - no descriptor at all - has
                    // actually run dry.
                    if self.descriptor.is_none() {
                        self.source_eof = true;
                        continue;
                    }
                    return;
                };
                match source.next_planar() {
                    Ok(Some(planes)) => {
                        converter.push(planes, staging);
                        Ok(true)
                    }
                    Ok(None) => Ok(false),
                    Err(error) => Err(error),
                }
            };
            match decoded {
                Ok(true) => {}
                // M7 §3.3: a station has no EOF. A body that ends is a
                // disconnect, and `source_ended` is the single exit for it.
                Ok(false) if self.is_indefinite() => {
                    self.source_ended(RemoteFailure::LiveEnded);
                    return;
                }
                Ok(false) => self.source_eof = true,
                Err(error) if is_retired_read(&error) => {
                    // Not EOF and not a fault: a stop, seek or shutdown
                    // retired this read. The decoder is now at an arbitrary
                    // point, so the source is retired with it and the
                    // interrupt the loop is about to act on decides what
                    // happens next (§8).
                    self.retire_remote_source();
                    return;
                }
                // After the retirement arm above, never before it: a
                // cancellation is never a disconnect (M7 §6.1).
                Err(error) if self.is_indefinite() => {
                    let failure =
                        remote_cause(&error).unwrap_or_else(|| RemoteFailure::Transport {
                            operation: Operation::Read,
                            detail: format!("decoding failed: {error}"),
                        });
                    self.source_ended(failure);
                    return;
                }
                // M10 §5 step 6: while an attempt primes a remote source it
                // has not committed to, the source's death is the attempt's
                // failure. A local file keeps M1's drain-what-it-has rule
                // below, even while `restore` primes it. The source is left
                // for `abandon_attempt` to drop: dropping it here retires the
                // shared interrupt, and `prime_attempt` would read that as a
                // cancellation, retrying at once with no backoff or budget.
                Err(error) if self.attempting && self.source_is_remote() => {
                    self.attempt_failure = Some(error);
                    return;
                }
                Err(error) => {
                    // M10 §3: an eligible finite episode recovers rather than
                    // failing. Everything else takes the path below unchanged.
                    if let Some(failure) = self.recoverable(&error) {
                        self.connection_lost(failure);
                        return;
                    }
                    match (remote_cause(&error), self.source_is_remote()) {
                        // §7: never reinterpret a status failure, a malformed
                        // range, a timeout or a truncated body as clean source
                        // EOF.
                        (Some(failure), _) => self.fail_with(format!("{failure}"), Some(failure)),
                        // H8: "malformed audio cannot become successful
                        // completion". A body that transferred perfectly and
                        // decoded to garbage has no remote cause at all, so
                        // keying on one alone lets exactly the case H8 names
                        // drain to EndOfTrack and mark the episode complete. A
                        // remote attempt that cannot finish decoding is a
                        // failed attempt, whatever the transport did.
                        (None, true) => self.fail_with(format!("decoding failed: {error}"), None),
                        // Local files keep M1's contract: a decode error late
                        // in a file the listener already heard most of drains
                        // what it has rather than discarding the session.
                        // `tests/decode_fixtures.rs` pins this.
                        (None, false) => {
                            self.source_eof = true;
                            self.warn(format!("decoding stopped early: {error}"));
                        }
                    }
                }
            }
        }
    }

    /// The single exit from `pump_audio` for indefinite media (M7 §5.2).
    ///
    /// While `fresh_open` is priming, the source has not been adopted yet, so
    /// its death is that attempt's failure and the caller decides what it
    /// means. Otherwise the established session has lost its station.
    ///
    /// The attempt's source is left for `abandon_attempt` to drop, as on the
    /// finite path: dropping it here retires the shared interrupt, which
    /// `fresh_open` reads as a cancellation and the loop retries at once,
    /// with no backoff and no budget.
    fn source_ended(&mut self, failure: RemoteFailure) {
        if self.attempting {
            self.attempt_failure = Some(failure.into());
            return;
        }
        if retryable(&failure, Continuity::Indefinite) {
            self.enter_reconnecting(failure);
        } else {
            self.fail_with(format!("{failure}"), Some(failure));
        }
    }

    /// M10 §3: the failure a finite read error recovers from, or `None` when
    /// it takes today's path. Only a playing, remote, finite session on a
    /// range-capable server recovers, and only from a failure the retry
    /// table calls retryable for finite media.
    ///
    /// `Unknown` seek support qualifies: the server already takes ranges and
    /// only the demuxer is untried, which is every episode played from zero
    /// without a seek. The attempt's reseek is that trial; a demuxer that
    /// refuses it fails the session, as it would have without recovery.
    fn recoverable(&self, error: &PlaybackError) -> Option<RemoteFailure> {
        let eligible = self.state == PlaybackState::Playing
            && self.source_is_remote()
            && self.capabilities.continuity == Continuity::Finite
            && self.capabilities.seek != SeekSupport::Unsupported;
        if !eligible {
            return None;
        }
        remote_cause(error).filter(|failure| retryable(failure, Continuity::Finite))
    }

    /// M10 §3: an eligible finite episode lost its connection while playing.
    /// The transport stays up so the ring plays out, as for a station.
    fn connection_lost(&mut self, failure: RemoteFailure) {
        // Before `enter_reconnecting` retires the decoder that knows it.
        // Established only: an estimate is not a ceiling (`clamp_target`).
        self.recovery_duration = self
            .source
            .as_ref()
            .and_then(|source| established_duration(source.metadata()));
        // M10 §3: a pause submitted but not yet dispatched raised the freeze
        // level, and the hook may already have announced `Paused`. The
        // listener asked to pause, so land there rather than reconnecting.
        if self.source_interrupt.is_frozen() {
            self.pause_closing();
            return;
        }
        self.enter_reconnecting(failure);
    }

    /// End of track fires only once every pushed frame has been played, which
    /// the timeline reports only after the final span's predicted play time
    /// has passed - not when the ring merely empties.
    fn check_end_of_track(&mut self) {
        // M7 §3.3: a station has no end.
        if self.is_indefinite() {
            return;
        }
        if self.state != PlaybackState::Playing || !self.decoder_drained {
            return;
        }
        let Some(rate) = self.config.as_ref().map(|c| c.sample_rate) else {
            return;
        };
        let now = self.output.now();
        // No call here reaches the decoder, so nothing risks a blocked read
        // while this is held (Ruling 3 is about `pump_audio`, not this).
        let landed_anchor = {
            let mut guard = lock(&self.transport);
            let Some(core) = guard.as_mut() else {
                return;
            };
            if core.timeline.played_frames(now) < self.pushed_total {
                return;
            }
            let anchor = core.anchor;
            let TransportCore {
                handshake,
                timeline,
                ..
            } = core;
            let mut pump = || std::thread::sleep(PUMP_NAP);
            // The outcome is deliberately ignored, exactly as before this
            // moved behind a lock: a timed-out park does not prevent
            // EndOfTrack from being reported - the decoder has genuinely run
            // dry either way.
            let _ = handshake.park(timeline, &mut pump, DEADLINE);
            anchor
        };
        self.position = landed_anchor + frames_to_duration(self.pushed_total, rate);
        // §9: a finite container can finish decoding before the body is
        // consumed. A truncated or timed-out tail cannot set completed
        // status.
        if let Some(failure) = self.confirm_remote_completion() {
            // `Cancelled` here means a
            // stop, seek or shutdown retired the confirmation read, not that
            // the tail failed to confirm anything. Reporting `Failed` would
            // be announcing a fault over an operation that is about to be
            // torn down anyway - the interrupt that caused this is still set
            // and the loop's next pass turns it into the transition the user
            // actually asked for, exactly as every other cancellation in this
            // file is handled.
            if matches!(failure, RemoteFailure::Cancelled) {
                return;
            }
            self.fail_with(format!("{failure}"), Some(failure));
            return;
        }
        let session_rev = self.session_rev;
        let position = self.position;
        self.emit(PlaybackEvent::EndOfTrack {
            session_rev,
            position,
            // R4 (Task 6): the terminal position above is the landed anchor
            // plus decoded frames, so it carries the anchor's provenance
            // unchanged - completion is evidence the *body* finished, not
            // that the anchor was right, and the session policy needs this
            // to keep an estimated completion from promoting its timestamp.
            provenance: self.position_provenance,
        });
        self.set_state(PlaybackState::Ended);
    }

    /// The bounded tail read §9 requires before a track may be called
    /// complete, for a remote source. `None` for a local one, which has
    /// nothing to confirm - the filesystem does not truncate mid-read.
    ///
    /// `HttpMediaSource::confirm_complete` cannot be reached through
    /// `DecodedSource`: Symphonia's `MediaSourceStream` erases its inner
    /// `Box<dyn MediaSource>` with no accessor back to the concrete type, and
    /// `MediaSource` carries no `Any` bound to downcast through even if it
    /// did. What that method actually reads from is not the source object at
    /// all, though - it is the shared `SourceInterrupt`'s own buffer and
    /// terminal outcome, keyed by generation, and the worker already holds
    /// the exact same `Arc<SourceInterrupt>` every `HttpMediaSource` this
    /// session ever opened was handed. A fresh `ByteChannel` built over it
    /// reads that same state directly, with no detour through the source -
    /// reproducing `confirm_complete`'s own read-and-classify loop rather
    /// than reaching for it.
    fn confirm_remote_completion(&mut self) -> Option<RemoteFailure> {
        if !self.source_is_remote() {
            return None;
        }
        // `Duration::default()` is zero, which would
        // make `demanded >= stall` fire on the very first slice and report a
        // stall the server never caused. `source_is_remote()` above means an
        // `HttpService` was necessarily installed to have opened this source
        // in the first place, so `None` here is unreachable in practice; the
        // fallback exists only so a defensive read of this method is never
        // wrong, not because it should ever be taken.
        let stall = lock(&self.http)
            .as_ref()
            .map(|service| service.limits().stall)
            .unwrap_or(Limits::default().stall);
        let channel = ByteChannel::new(Arc::clone(&self.source_interrupt));
        let hook: Arc<dyn WaitHook> = self.service.clone();
        let mut scratch = [0u8; 64 * 1024];
        loop {
            match channel.read(&mut scratch, hook.as_ref(), stall) {
                ReadOutcome::Bytes(_) => continue,
                ReadOutcome::Eof => return None,
                ReadOutcome::Retired => return Some(RemoteFailure::Cancelled),
                ReadOutcome::Failed(failure) => return Some(failure),
            }
        }
    }

    // --------------------------------------------------------------- commands

    fn dispatch(&mut self, command: PlaybackCommand) {
        match command {
            PlaybackCommand::Load {
                request,
                media,
                source,
                resume,
            } => self.load(request, media, source, resume),
            PlaybackCommand::Play => self.play(false),
            PlaybackCommand::PlayLoaded { request } => {
                if self.adopted_load == Some(request) && self.state == PlaybackState::Paused {
                    self.play(true);
                }
            }
            PlaybackCommand::Pause => self.pause(),
            PlaybackCommand::TogglePause => match self.state {
                PlaybackState::Playing | PlaybackState::Reconnecting => self.pause(),
                _ => self.play(false),
            },
            PlaybackCommand::SeekTo(target) => self.seek_to(target),
            PlaybackCommand::SeekBy(delta) => {
                // M10 §7: presses accumulate on a stored intent.
                let base = self.pending.map_or(self.position, PendingResume::target);
                let step = Duration::from_secs(delta.unsigned_abs());
                let target = if delta >= 0 {
                    base.saturating_add(step)
                } else {
                    base.saturating_sub(step)
                };
                self.seek_to(target);
            }
            PlaybackCommand::Restart => self.restart(),
            PlaybackCommand::SetVolume(volume) => {
                self.volume = volume;
                if let Some(link) = self.link.as_ref() {
                    link.set_gain(volume.as_gain());
                }
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::VolumeChanged {
                    session_rev,
                    volume,
                });
            }
            PlaybackCommand::Stop => self.do_stop(),
            PlaybackCommand::Shutdown => self.shutting_down = true,
        }
    }

    fn load(
        &mut self,
        request: LoadRequestId,
        media: MediaId,
        source: SourceLocation,
        resume: ResumeIntent,
    ) {
        // Whatever this replaces, its outage goes with it.
        self.outage = None;
        self.capture_and_teardown();
        // The source the handle was reasoning about is gone and the incoming
        // one is not known yet, so the handle gets the conservative answer
        // until `set_capabilities` below says otherwise (M7 §6.1): a seek
        // submitted in the gap retires a generation that is being replaced
        // anyway.
        self.traits.indefinite.store(false, Ordering::Release);
        self.traits.seek_unsupported.store(false, Ordering::Release);
        self.source = None;
        self.session_rev += 1;
        self.media = Some(media.clone());
        self.pending = None;
        self.recovery_duration = None;
        // This load's media is not yet held until `Loaded` says so, and its
        // token is now the one in flight - both before `set_state(Loading)`
        // reads `self.loading` for the announcement below (M5 §6).
        self.adopted_load = None;
        self.loading = Some(request);
        // A caller-decided start is already the position that will be asked
        // for, so it is pinned BEFORE opening: if the load fails, Failed must
        // carry it so a retry can resume there. Zeroing here loses it for
        // every failure path below. A `Candidate` has no position to pin yet -
        // deciding one needs the duration only the decoder can report, so it
        // waits until the source has actually opened, below.
        //
        // `self.position` alone is enough: no transport exists at this point
        // (the teardown above just cleared it), and `open_transport` reads
        // `self.position` as the new generation's anchor when it builds one,
        // below.
        if let ResumeIntent::StartAt(target) = resume {
            self.position = target;
        }
        // Reset for the new session, unconditionally: a stale `Estimated`
        // left over from the session this load just tore down must not
        // survive into one that never seeks at all (`start_at == 0` below).
        // The resume seek further down is the only thing in this method that
        // may set it back to `Estimated`.
        self.position_provenance = PositionProvenance::Established;
        self.degraded = false;
        self.set_state(PlaybackState::Loading);
        // Set unconditionally, success or failure: a failed remote open still
        // leaves something for a later `Play` to retry against (§9's "one
        // explicit reopen"), and `shutdown` is the only thing that clears it.
        self.descriptor = Some(source.clone());

        let prepared = match prepare(&source, &self.prepare_context()) {
            Ok(prepared) => prepared,
            // A stop or shutdown landing during
            // this open must not be reported as `Failed` - `self.state` is
            // still `Loading` here (set above), so leaving it alone is what
            // lets the very next pass's `do_stop` actually run instead of
            // early-returning on a `Failed` it did not ask for.
            Err(error) if is_cancelled(&error) => {
                self.cancel_load();
                return;
            }
            Err(error) => {
                self.fail_from(error);
                return;
            }
        };
        let decoded = prepared.source;
        self.set_capabilities(prepared.capabilities);

        // M7 §5: a station starts where its connection joined, which is zero
        // by definition. Decided before the match below rather than inside
        // it, so a stale candidate carried into a station load can never be
        // announced as `ResumeUnavailable` - there is no position it was
        // denied, only one that never applied.
        let resume = if self.is_indefinite() {
            self.position = Duration::ZERO;
            ResumeIntent::StartAt(Duration::ZERO)
        } else {
            resume
        };

        // A `Candidate` is decided now, against the duration this decoder just
        // reported - the same rule `decide_resume` applies wherever a
        // duration is in hand (G3), so a worker-resolved resume and an
        // application-resolved one can never land somewhere the checkpoint
        // never said.
        let (start_at, disposition) = match resume {
            ResumeIntent::StartAt(target) => (target, StartDisposition::Fresh),
            ResumeIntent::Candidate(candidate) => {
                let known_duration = decoded.metadata().duration.map(|value| KnownDuration {
                    value,
                    provenance: decoded.metadata().duration_provenance,
                });
                let decision = decide_resume(Some(candidate), known_duration);
                let target = decision.start_at();
                // §10: a positive candidate a non-seekable source cannot
                // establish. Starting at zero silently would be exactly the
                // reset the invariant forbids, so this is announced rather
                // than assumed - playback still starts, just from zero, with
                // the entry protected against being overwritten by it.
                if target > Duration::ZERO && self.capabilities.seek == SeekSupport::Unsupported {
                    self.position = Duration::ZERO;
                    (
                        Duration::ZERO,
                        StartDisposition::ResumeUnavailable { retained: target },
                    )
                } else {
                    self.position = target;
                    let disposition = match decision {
                        ResumeDecision::Completed => StartDisposition::CompletedReplay,
                        ResumeDecision::Resume(_) | ResumeDecision::Unvalidated(_) => {
                            StartDisposition::Resumed
                        }
                        ResumeDecision::NoEntry
                        | ResumeDecision::AtStart
                        | ResumeDecision::DegenerateEnd
                        | ResumeDecision::StalePastEnd => StartDisposition::Fresh,
                    };
                    (target, disposition)
                }
            }
            // The estimate always wins as the
            // seek target, unconditionally — it is a preference between two
            // already-known locations decided upstream in
            // `resume::resume_intent`, not a duration-validated choice,
            // so nothing here re-runs `decide_resume` against the decoded
            // duration the way `Candidate` does. `established` is reported
            // exactly as `resume::resume_intent` computed it — R8's "never
            // established" vs. "established at the start" distinction
            // already lives there, upstream of this call, so it is never
            // re-derived or re-validated here either.
            ResumeIntent::EstimatedCandidate {
                target,
                established,
            } => {
                // §10, carried from the branch above: a non-seekable source
                // cannot honour an estimated target any more than an
                // established one. Reported and protected exactly the same
                // way — `ResumeUnavailable` is not a disposition this
                // amendment adds a second version of.
                if target > Duration::ZERO && self.capabilities.seek == SeekSupport::Unsupported {
                    self.position = Duration::ZERO;
                    (
                        Duration::ZERO,
                        StartDisposition::ResumeUnavailable { retained: target },
                    )
                } else {
                    self.position = target;
                    (target, StartDisposition::ResumedEstimated { established })
                }
            }
        };

        self.source = Some(decoded);
        if start_at > Duration::ZERO {
            // A demuxer seek this source had not yet proven is proven here,
            // so `Loaded`'s own `capabilities` already reflects it (§6).
            match self.land(start_at, Approach::Preserve) {
                Landed::At(_) => {}
                // Abandon the load, leaving no decoder open. The interrupt
                // still stands and the loop's next pass acts on it.
                Landed::Cancelled => {
                    self.source = None;
                    self.cancel_load();
                    return;
                }
                Landed::Failed(error) => {
                    self.source = None;
                    self.fail_from(error);
                    return;
                }
            }
        }
        let session_rev = self.session_rev;
        let position = self.position;
        let capabilities = self.capabilities;
        let metadata = self
            .source
            .as_ref()
            .map(|source| source.metadata().clone())
            .unwrap_or_default();
        // Takes `loading`: this outcome closes it. `adopted_load` now holds
        // the same token until the next `load` clears it (M5 §6, Decision 2).
        self.loading = None;
        self.adopted_load = Some(request);
        self.emit(PlaybackEvent::Loaded {
            session_rev,
            request,
            media,
            metadata,
            capabilities,
            position,
            disposition,
        });
        match self.open_transport(false) {
            Ok(()) => self.set_state(PlaybackState::Paused),
            Err(error) => self.fail(format!("cannot open the audio device: {error}")),
        }
    }

    /// `primed`: whether this Play is the `PlayLoaded` of the load that primed
    /// the current transport. Only that one may release it; every other Play
    /// on a station rejoins the live edge instead (M7 §5.3).
    fn play(&mut self, primed: bool) {
        match self.state {
            // M7 §5.3: a plain `Play`, a `TogglePause` or a `PlayLoaded` for
            // some other load opens fresh rather than releasing a transport
            // primed against a connection that has since moved on.
            PlaybackState::Paused if self.is_indefinite() && !primed => self.restore(),
            // `Reconnecting` alongside `Playing`: playback is already what
            // the listener asked for, the transport is simply waiting on the
            // station (M7 §5).
            PlaybackState::Playing | PlaybackState::Reconnecting => {}
            // Play from Ended never restarts implicitly: the application has
            // to say `Restart`, so that "play" cannot silently lose the fact
            // that the track finished.
            PlaybackState::Ended => {
                self.warn("the track has ended; restart it to play it again".into())
            }
            // §9: one explicit reopen at the preserved position. The
            // listener asked for it, so it is not the automatic retry §1.3
            // rules out - and it is one attempt, not a loop: a second
            // failure lands back in `Failed` and the next `Play` tries once
            // again, driven by the listener each time.
            //
            // The state moves to `Loading` *before* `restore()` runs:
            // `fail_with`'s very first statement
            // returns early when `self.state == Failed`, which exists so a
            // repeating fatal fault does not re-announce itself every loop
            // iteration - but a listener-driven retry is not that, and a
            // failing reopen left `self.state` at `Failed` would hit that
            // exact guard and emit nothing at all for an explicit user
            // action, which is worse than the duplicate the guard was
            // written to prevent. `Loading` is also the honest state while
            // a reopen is in flight.
            PlaybackState::Failed if self.remote_descriptor().is_some() => {
                self.set_state(PlaybackState::Loading);
                self.restore();
            }
            PlaybackState::Failed => {
                self.warn("playback failed; load the media again before playing".into())
            }
            PlaybackState::Idle => self.warn("nothing is loaded".into()),
            // Resuming a parked transport is the same generation released
            // again: nothing was discarded, so nothing has to be rebuilt.
            PlaybackState::Paused if lock(&self.transport).is_some() && self.pending.is_none() => {
                // Facts before transport (Ruling 1), even though neither lock
                // here is ever held while the other is taken: consistent
                // order is one less thing a future reader has to check.
                //
                // If the hook parked for a freeze that has not yet thawed, an
                // explicit `Play` from the application takes over: the flag
                // no longer describes reality once this dispatch releases the
                // transport itself (the counterpart to `pause`'s check of the
                // same flag, below).
                lock(&self.facts).frozen_by_hook = false;
                {
                    let mut guard = lock(&self.transport);
                    if let Some(core) = guard.as_mut() {
                        core.release();
                    }
                }
                self.set_state(PlaybackState::Playing);
            }
            PlaybackState::Loading | PlaybackState::Paused | PlaybackState::Stopped => {
                self.restore()
            }
        }
    }

    /// Restore playback from a torn-down transport, at the preserved position
    /// or at a target stored while stopped - the one case where resuming
    /// establishes a new position rather than preserving one.
    fn restore(&mut self) {
        // M7 §5.2, before every seek-shaped gate below: for a station,
        // `position` is listening time, never a target. The one sequence
        // opens a fresh connection and keeps it.
        if self.is_indefinite() {
            if self.pending.take().is_some() {
                self.warn("a stored seek target was dropped: live media cannot seek".into());
            }
            match self.fresh_open() {
                Ok(()) => {}
                Err(error) if is_cancelled(&error) => {}
                // An explicit Play is one attempt (M7 §7): it fails honestly.
                Err(error) => self.fail_from(error),
            }
            return;
        }
        // §9: a source that cannot seek cannot restore a nonzero position,
        // and starting at zero silently would be exactly the reset the
        // milestone's invariant forbids. Checked *before* any reopen:
        // `self.capabilities` already answers this
        // from the source's last time open - a stop-then-play cycle reopens
        // the same location and gets the same answer - so opening a live
        // fetch only to reject and abandon it here would leak the
        // connection until shutdown or a new load.
        if self.position > Duration::ZERO && self.capabilities.seek == SeekSupport::Unsupported {
            self.reject_seek("this server cannot resume; the position is kept".into());
            self.set_state(PlaybackState::Stopped);
            return;
        }
        // A remote source the worker retired - by a stop, or by a failure -
        // is reopened here, not treated as absent. A no-op when a decoder is
        // already live, so the local path is unchanged.
        match self.ensure_source_open() {
            Ok(_) => {}
            // The state a caller left this in
            // (`Loading`, from `play`'s `Failed` arm) survives untouched, so
            // a stop landing during this reopen reaches the next pass's
            // `do_stop` instead of being swallowed by a `Failed` it did not
            // ask for.
            Err(error) if is_cancelled(&error) => return,
            Err(error) => {
                self.fail_from(error);
                return;
            }
        }
        if self.source.is_none() {
            self.warn("nothing is loaded".into());
            return;
        }
        if lock(&self.transport).is_some() {
            self.capture_position();
        }
        // M10 §4: read, not taken. A reseek, install or priming failure
        // leaves the intent for the next Space and for the checkpoint.
        let previous = (self.position, self.position_provenance);
        let target = self.pending.map_or(self.position, PendingResume::target);
        let landing = match self.land(target, Approach::Preserve) {
            Landed::At(landing) => landing,
            // A stop or a shutdown arrived mid-refinement. The preserved
            // position still stands; the interrupt is handled by the loop.
            Landed::Cancelled => return,
            Landed::Failed(error) => {
                self.fail(format!("cannot resume at {target:?}: {error}"));
                return;
            }
        };
        // M10 §5: the same commit check as a recovery attempt, so a priming
        // failure fails honestly instead of announcing Playing over a
        // `Failed` that `pump_audio` already set.
        match self.prime_attempt() {
            Ok(()) => {
                self.start_running();
                self.announce_playing();
                self.complete_pending(landing);
            }
            Err(error) => {
                (self.position, self.position_provenance) = previous;
                if !is_cancelled(&error) {
                    self.fail_attempt(error);
                }
            }
        }
    }

    /// Space is one attempt: when it fails, say what failed. A remote cause
    /// travels typed; anything else is the device or the decoder.
    fn fail_attempt(&mut self, error: PlaybackError) {
        if matches!(error, PlaybackError::Remote(_)) || remote_cause(&error).is_some() {
            self.fail_from(error);
        } else {
            self.fail(format!("cannot start playback: {error}"));
        }
    }

    /// M10 §4: a stored intent is cleared only once its landing is installed,
    /// and only then reported: `SeekCompleted` for a seek, `RestartEstablished`
    /// for a restart. Emitting at store time would claim a landing no decoder
    /// had confirmed.
    pub(super) fn complete_pending(&mut self, landing: Landing) {
        let Landing { actual, truncated } = landing;
        let session_rev = self.session_rev;
        let provenance = self.position_provenance;
        match self.pending.take() {
            Some(PendingResume::Seek(requested)) => {
                // §11: requested/actual seek.
                tracing::debug!(?requested, ?actual, "stored seek target confirmed");
                self.emit(PlaybackEvent::SeekCompleted {
                    session_rev,
                    requested,
                    actual,
                    refinement_truncated: truncated,
                    provenance,
                });
            }
            Some(PendingResume::Restart) => {
                let position = self.position;
                self.emit(PlaybackEvent::RestartEstablished {
                    session_rev,
                    position,
                    provenance,
                });
            }
            None => {}
        }
    }

    /// Park the callback and leave everything else exactly as it is. The ring
    /// keeps its audio, the callback keeps counting from where it stopped, and
    /// the timeline keeps its floor, so resuming is a single `release`.
    fn pause(&mut self) {
        // First, ahead of the `Playing`-only guard below: a station never
        // parks, and neither does a finite session in recovery or one whose
        // recovery-time pause already retired its source (M10 §7).
        if self.is_indefinite()
            || self.state == PlaybackState::Reconnecting
            || self.lost_source_while_playing()
        {
            self.pause_closing();
            return;
        }
        if self.state != PlaybackState::Playing {
            return;
        }
        // §7.5's table: a pause clears the outage, a park included. A
        // recovered connection paused inside its stability window would
        // otherwise carry the old `started` into the next drop and give up.
        self.outage = None;
        // Idempotent with respect to the hook: if it already parked the
        // transport for a freeze and announced `Paused` itself, that
        // announcement already stands, so this only adopts the state
        // rather than parking (redundantly) and re-emitting. Through
        // `adopt_state`, never a bare field write, so `pausable` flips
        // with the state (H1: a stuck-`true` flag made Space unresumable).
        if lock(&self.facts).frozen_by_hook {
            self.adopt_state(PlaybackState::Paused);
            return;
        }
        let parked = {
            let mut guard = lock(&self.transport);
            match guard.as_mut() {
                Some(core) => {
                    let TransportCore {
                        handshake,
                        timeline,
                        ..
                    } = core;
                    let mut pump = || std::thread::sleep(PUMP_NAP);
                    handshake.park(timeline, &mut pump, DEADLINE)
                }
                None => Ok(()),
            }
        };
        if parked.is_err() {
            // A park that never gets acknowledged means the device is not
            // running. Announcing Paused here would leave a dead transport
            // behind a state that claims it can resume, so recover instead and
            // land parked - or fail with the position preserved.
            match self.rebuild("the audio device stopped responding while pausing", false) {
                // The rebuild's priming lost the connection: the recovery, or
                // the failure, now owns the session (M10 §3).
                Ok(()) if self.state != PlaybackState::Playing => {}
                Ok(()) => {
                    self.reconcile_frozen_by_hook();
                    self.set_state(PlaybackState::Paused);
                }
                Err(error) if is_cancelled(&error) => {}
                Err(error) => self.fail(format!("cannot pause: {error}")),
            }
            return;
        }
        // `submit_pause` leaves the freeze level up for as long as the pause
        // lasts. If the hook parked first, inside a blocked read, the early
        // return above already handles it. If this dispatch parked instead,
        // nothing has told the hook - and a read that blocks while paused (a
        // seek's priming read) would see `frozen && !frozen_by_hook`, park
        // the transport again and announce `Paused` a second time. `play`'s
        // resume branch clears this same flag; this is its counterpart.
        self.reconcile_frozen_by_hook();
        self.set_state(PlaybackState::Paused);
    }

    /// M10 §7's race: `submit_pause` read `recovering` just before the
    /// attempt committed, so its retirement landed on the generation that is
    /// now playing. Parking a transport with no source behind it would strand
    /// the resume; close instead, and let Space reopen.
    fn lost_source_while_playing(&self) -> bool {
        self.state == PlaybackState::Playing
            && self.source_is_remote()
            && (self.source.is_none() || self.source_interrupt.is_retired())
    }

    /// M7 §6.2, M10 §7. A pause that keeps no connection: a station's, and a
    /// finite session's during recovery. Both pause routes end here, the
    /// dispatched `Pause` and the one where the hook parked first and
    /// already announced `Paused`. Position and `pending` survive; the next
    /// Play runs `restore()` (or `fresh_open` for a station).
    fn pause_closing(&mut self) {
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Reconnecting
        ) {
            return;
        }
        // §6.2 step 4: a pause cancels any pending reconnect attempt and
        // clears the outage. Cancelling it is the state change alone - the
        // loop only attempts while `Reconnecting` - and clearing it is what
        // gives the next Play a fresh budget rather than a spent one.
        self.outage = None;
        // No transport will remain for a thaw to release, and
        // `WaitService::park` answers `true` with none — so both the flag and
        // the level go, or the next thaw announces `Playing` over nothing.
        let announced = std::mem::take(&mut lock(&self.facts).frozen_by_hook);
        self.source_interrupt.thaw();
        self.capture_and_teardown();
        self.source_interrupt.retire();
        self.retire_remote_source();
        self.session_rev += 1;
        if announced {
            // The hook already announced this pause from inside the blocked
            // read; adopt the state quietly — `adopt_state` still clears
            // `recovering` and flips `pausable`, which a bare field write
            // would skip (H1).
            self.adopt_state(PlaybackState::Paused);
        } else {
            self.set_state(PlaybackState::Paused);
        }
    }

    /// Set `frozen_by_hook` to match reality after this dispatch parked the
    /// transport on its own, so the next blocked read does not act on a
    /// stale mismatch.
    ///
    /// Conditional on the freeze level actually being active
    /// (`source_interrupt.is_frozen()`), not unconditional: a local pause
    /// dispatched the ordinary way, with no `submit_pause` and so no freeze
    /// level ever raised, must leave this exactly as it was. Setting it
    /// unconditionally trips `service_as`'s *other* branch instead -
    /// `!frozen && frozen_by_hook` reads as "release" - undoing the pause in
    /// the next blocked read.
    fn reconcile_frozen_by_hook(&mut self) {
        if self.source_interrupt.is_frozen() {
            lock(&self.facts).frozen_by_hook = true;
        }
    }

    fn do_stop(&mut self) {
        if matches!(
            self.state,
            PlaybackState::Idle | PlaybackState::Stopped | PlaybackState::Failed
        ) {
            return;
        }
        self.outage = None;
        self.capture_and_teardown();
        // §9: "retire fetch, wake reads, discard transport and remote
        // decoder; keep identity/source/position". `capture_and_teardown`
        // just did the transport half; this does the remote half.
        self.source_interrupt.retire();
        self.retire_remote_source();
        self.session_rev += 1;
        self.set_state(PlaybackState::Stopped);
    }

    /// M10 §7: a finite session holding its intent with nothing open: in
    /// recovery, or paused out of one (no decoder, no transport). Seek
    /// support is already proven for any session that got here, so seeks and
    /// restarts are stored without network I/O.
    fn holds_intent_offline(&self) -> bool {
        if self.is_indefinite() || !self.source_is_remote() {
            return false;
        }
        match self.state {
            PlaybackState::Reconnecting => true,
            PlaybackState::Paused => self.source.is_none() && lock(&self.transport).is_none(),
            _ => false,
        }
    }

    /// Store `intent` and announce it, so the display and the checkpoint
    /// follow. The outage, backoff and schedule are untouched.
    fn store_intent(&mut self, intent: PendingResume) {
        self.pending = Some(intent);
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::SeekTargetStored {
            session_rev,
            target: intent.target(),
        });
    }

    fn seek_to(&mut self, requested: Duration) {
        if self.state == PlaybackState::Failed {
            self.reject_seek("playback failed; load the media again".into());
            return;
        }
        if self.holds_intent_offline() {
            let target = self
                .recovery_duration
                .map_or(requested, |duration| requested.min(duration));
            self.store_intent(PendingResume::Seek(target));
            return;
        }
        // A source known to be unsupported is rejected before any reopen,
        // playing or stopped alike: checked
        // before `ensure_source_open` because `self.capabilities` already
        // answers this from the source's last time open - a retired remote
        // source's location hasn't changed, so opening a live fetch only to
        // reject and abandon it here would leak the connection until
        // shutdown or a new load. A source this project has already
        // classified `Unsupported` has no `HttpMediaSource::seek()` to fall
        // back on either way, so Symphonia's own recovery is to read
        // forward on the same connection - which cannot be undone if the
        // target turns out to be unreachable, and would otherwise turn a
        // plain refusal into an unrecoverable `Failed` when the position
        // cannot be restored. §10's "reject unsupported stopped seeks before
        // SeekTargetStored" is the specific case; this is the general one.
        if self.capabilities.seek == SeekSupport::Unsupported {
            self.reject_seek("this source cannot seek".into());
            return;
        }
        // A remote source the worker retired is reopened here, not treated
        // as absent. `ensure_source_open` is a no-op when a decoder is
        // already live, so the local path is unchanged. This has to come
        // before the `source.is_none()` guard below: after a stop or a
        // retired seek there is no decoder, and the guard would otherwise
        // reject the seek as "nothing is loaded" on a source whose identity,
        // descriptor and position the worker is still holding.
        if let Err(error) = self.ensure_source_open() {
            // A stop or shutdown landing
            // during this reopen is a cancellation, not a validation
            // failure - §8's "never `SeekRejected`, which would misreport a
            // cancellation" applies here exactly as it already does to the
            // seek proper, below.
            if is_cancelled(&error) {
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekCancelled {
                    session_rev,
                    requested,
                });
                return;
            }
            self.reject_seek(format!("{error}"));
            return;
        }
        if self.source.is_none() {
            self.reject_seek("nothing is loaded".into());
            return;
        }
        let target = self.clamp_target(requested);
        match self.state {
            // Deliberately not `SeekCompleted`: with no transport running,
            // nothing has validated this target yet.
            PlaybackState::Idle | PlaybackState::Stopped => {
                // §6: until conclusive evidence exists, publish Unknown
                // and verify on demand. A stopped seek needs the answer
                // before it may store a target M2 treats as durable.
                if self.capabilities.seek == SeekSupport::Unknown {
                    match self.verify_seek_support() {
                        Ok(true) => {}
                        // A genuine demuxer refusal: this and only this
                        // means "this source cannot seek".
                        Ok(false) => {
                            self.reject_seek("this source cannot seek".into());
                            return;
                        }
                        Err(error) if is_cancelled(&error) => {
                            let session_rev = self.session_rev;
                            self.emit(PlaybackEvent::SeekCancelled {
                                session_rev,
                                requested: target,
                            });
                            return;
                        }
                        // A typed remote failure (a 416, `ResourceChanged`, a
                        // stall...) is reported as what it actually is,
                        // never flattened into the capability verdict above.
                        Err(error) => {
                            self.reject_seek(format!("{error}"));
                            return;
                        }
                    }
                }
                self.pending = Some(PendingResume::Seek(target));
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekTargetStored {
                    session_rev,
                    target,
                });
                return;
            }
            _ => {}
        }
        let playing = self.state == PlaybackState::Playing;
        self.capture_position();
        let preserved = self.position;
        // Deliberately no pre-emptive `begin()` here.
        // The gate above already refused every source that would reach this
        // point without a `MediaSource::seek()` call to answer it (Symphonia
        // reads forward on the *same* generation for one it cannot really
        // seek), so every source still reachable here is byte-seekable and
        // `HttpMediaSource::seek()` opens its own fresh generation as its
        // first action regardless. A `begin()` taken here instead would
        // retire whatever generation is *actually* live - including one a
        // short forward seek satisfies entirely from `MediaSourceStream`'s
        // own read-ahead buffer, with no `seek()` call at all - and nothing
        // would ever open a new fetch to replace it.
        match self.land(target, Approach::Seek) {
            Landed::At(landing) => {
                self.pending = None;
                if let Err(error) = self.reinstall(playing) {
                    if !is_cancelled(&error) {
                        self.fail(format!("cannot restart the audio device: {error}"));
                    }
                    return;
                }
                if self.state == PlaybackState::Ended {
                    self.set_state(PlaybackState::Paused);
                }
                let Landing { actual, truncated } = landing;
                // §11: requested/actual seek.
                tracing::debug!(requested = ?target, ?actual, truncated, "seek completed");
                let session_rev = self.session_rev;
                let provenance = self.position_provenance;
                self.emit(PlaybackEvent::SeekCompleted {
                    session_rev,
                    requested: target,
                    actual,
                    refinement_truncated: truncated,
                    provenance,
                });
            }
            // §8: an accepted seek always receives an outcome, even when a
            // stop, a shutdown or a newer seek retires it before it commits -
            // never silence, and never `SeekRejected`, which would misreport
            // a cancellation as a validation failure.
            Landed::Cancelled => {
                self.position = preserved;
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekCancelled {
                    session_rev,
                    requested: target,
                });
                // Best-effort restoration at the preserved position (§5.4):
                // one fresh bounded attempt, never the expired deadline this
                // seek may have just hit.
                if let Landed::At(_) = self.land(preserved, Approach::Preserve) {
                    let _ = self.reinstall(playing);
                }
            }
            Landed::Failed(error) => {
                // The captured value stands. The decoder is parked at an
                // arbitrary point inside the refinement, so the position has
                // to be re-established explicitly, never assumed.
                self.position = preserved;
                // §5.4: the same fresh-deadline recovery as the cancelled arm
                // above.
                match self.land(preserved, Approach::Preserve) {
                    // Cancelled again: the stop or shutdown that cancelled the
                    // seek is about to be handled, and it tears the decoder
                    // down anyway. The preserved position stands.
                    Landed::Cancelled => {}
                    Landed::At(_) => {
                        if let Err(reinstall_error) = self.reinstall(playing) {
                            if !is_cancelled(&reinstall_error) {
                                self.fail(format!(
                                    "cannot restart the audio device: {reinstall_error}"
                                ));
                            }
                            return;
                        }
                        self.reject_seek(format!("{error}"));
                    }
                    Landed::Failed(restore_error) => self.fail(format!(
                        "seek failed ({error}) and the decoder could not be restored ({restore_error})"
                    )),
                }
            }
        }
    }

    fn restart(&mut self) {
        // M7 §5.1: before `ensure_source_open`. The reopened-source branch
        // below would zero listening time and report a restart.
        if self.is_indefinite() {
            self.reject_seek("a live stream cannot restart".into());
            return;
        }
        // M10 §7: stored, never collapsed into `Seek(ZERO)`. The landing
        // reports `RestartEstablished`.
        if self.holds_intent_offline() {
            self.store_intent(PendingResume::Restart);
            return;
        }
        let reopened = match self.ensure_source_open() {
            Ok(reopened) => reopened,
            // Same rule as `restore`'s arm above
            // - a cancellation must not become `Failed`, or the stop that
            // caused it is swallowed by `do_stop`'s own early return.
            Err(error) if is_cancelled(&error) => return,
            Err(error) => {
                self.fail_from(error);
                return;
            }
        };
        if self.source.is_none() {
            self.warn("nothing is loaded".into());
            return;
        }
        if lock(&self.transport).is_some() {
            self.capture_position();
        }
        if reopened {
            // §9: "Restart: explicitly open from zero." A fresh reopen
            // already begins at byte zero - there is nothing to seek, and
            // asking a brand new `HttpMediaSource` to seek to the position
            // it is already at would only spend a network round trip
            // proving what opening it already established.
            self.position = Duration::ZERO;
            // A fresh open at byte zero is exact - nothing was estimated,
            // there was nothing to seek - so this is one of the two acts
            // that clears a sticky `Estimated` (§4.4's `RestartEstablished`
            // exit), not merely a position reset.
            self.position_provenance = PositionProvenance::Established;
            self.pending = None;
        } else {
            // Validate first: the transport is only started once the decoder
            // has actually landed at zero.
            match self.land(Duration::ZERO, Approach::Preserve) {
                Landed::At(_) => self.pending = None,
                Landed::Cancelled => return,
                Landed::Failed(error) => {
                    self.reject_seek(format!("{error}"));
                    return;
                }
            }
        }
        let before = self.state;
        match self.reinstall(true) {
            // M10 §3: the priming lost the connection, and the recovery or
            // the failure now owns the session. Announcing here would cover
            // it with a `Playing` that has no source. A recovery lands the
            // restart and reports it then.
            Ok(()) if self.state != before => {
                if self.state == PlaybackState::Reconnecting {
                    self.pending = Some(PendingResume::Restart);
                }
            }
            Ok(()) => {
                // G1: `restart()` is the only establishment that discards a
                // stored target and lands at zero with no `SeekCompleted`
                // ever emitted (D17), so it needs an event of its own for a
                // policy that has to tell an explicit restart apart from any
                // other establishment (Task 11 lifts checkpoint protection on
                // exactly this). Emitted only here, on the `Ok` arm: a
                // cancelled or failed `reinstall` announced nothing to begin
                // with, and must not announce a restart that did not land.
                let session_rev = self.session_rev;
                let position = self.position;
                self.emit(PlaybackEvent::RestartEstablished {
                    session_rev,
                    position,
                    // Task 6: the `reopened` branch above sets this
                    // Established unconditionally (a fresh open at byte
                    // zero is exact); the `else` branch reads it back from
                    // `land`'s own outcome, which for an MP3 restart that
                    // reuses an already-open decoder can still be
                    // `Estimated` (decode.rs's format check is
                    // target-independent). The session policy needs the
                    // real answer to decide whether this restart is one of
                    // the two acts that clears a sticky `Estimated`.
                    provenance: self.position_provenance,
                });
                self.announce_playing();
            }
            Err(error) if is_cancelled(&error) => {}
            Err(error) => self.fail(format!("cannot start playback: {error}")),
        }
    }

    /// The one landing (M9.4): seek the decoder to `target` and adopt the
    /// result - position, provenance, and the proof an unproven demuxer
    /// earns from any real seek (§6). Every caller that moves the decoder
    /// goes through here, so the rules cannot drift between them.
    ///
    /// The target is clamped to an established duration first: a target
    /// stored offline with no duration to clamp it to (M10 §7) meets the
    /// decoder's here, rather than failing past the end on every Space.
    /// A decoder already at the target is not seeked at all: nothing about
    /// the provenance changes, rather than a landing being manufactured that
    /// never happened. Nothing is written on `Cancelled` or `Failed`.
    ///
    /// Cancellable, so a stop or a shutdown is not left waiting for a
    /// refinement to finish; the deadline is fresh on every call (§5.4).
    pub(super) fn land(&mut self, target: Duration, approach: Approach) -> Landed {
        let target = self.clamp_target(target);
        let deadline = self.seek_deadline();
        let Some(source) = self.source.as_mut() else {
            return Landed::Failed(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no media is loaded".into(),
            });
        };
        let seeked = source.position() != target;
        let (actual, provenance, truncated) = if seeked {
            let budget = match approach {
                Approach::Seek => Some(SEEK_BUDGET),
                Approach::Preserve => None,
            };
            let (outcome, provenance) = match seek_bounded(
                source,
                &self.source_interrupt,
                &self.interrupt,
                deadline,
                target,
                budget,
            ) {
                Ok(landed) => landed,
                Err(error) if is_cancelled(&error) => return Landed::Cancelled,
                Err(error) => return Landed::Failed(error),
            };
            // `refinement_truncated` is false when refinement ran into the
            // end of the media, so a short landing is checked separately.
            let truncated = outcome.refinement_truncated
                || outcome.actual.saturating_add(RESUME_TOLERANCE) < target;
            (outcome.actual, provenance, truncated)
        } else {
            (target, self.position_provenance, false)
        };
        self.position = match approach {
            Approach::Seek => actual,
            Approach::Preserve => adopt_preserved(target, actual),
        };
        // A restart that lands at zero is exact, whatever provenance the
        // decoder reports for its format (M10 §4).
        self.position_provenance =
            if self.pending == Some(PendingResume::Restart) && actual == Duration::ZERO {
                PositionProvenance::Established
            } else {
                provenance
            };
        if seeked
            && self.capabilities.seek == SeekSupport::Unknown
            && let Some(source) = self.source.as_mut()
        {
            source.note_demuxer_proven();
            let capabilities = source.capabilities();
            self.set_capabilities(capabilities);
            self.emit_capabilities(capabilities);
        }
        Landed::At(Landing { actual, truncated })
    }

    fn clamp_target(&self, requested: Duration) -> Duration {
        match self
            .source
            .as_ref()
            .and_then(|source| established_duration(source.metadata()))
        {
            Some(duration) => requested.min(duration),
            None => requested,
        }
    }

    // -------------------------------------------------------------- remote

    /// Whether `descriptor` names an HTTP source. Reads `descriptor`, not
    /// `source`: a retired remote source has `source == None` but is still
    /// remote, and every gate that must tell "no remote source" from "a
    /// remote source between a stop and its reopen" apart needs this
    /// distinction.
    fn source_is_remote(&self) -> bool {
        matches!(self.descriptor, Some(SourceLocation::Http(_)))
    }

    /// The URL a remote `descriptor` names, for `play`'s `Failed` arm to key
    /// its one-explicit-reopen rule on without duplicating the match.
    fn remote_descriptor(&self) -> Option<&Url> {
        match &self.descriptor {
            Some(SourceLocation::Http(url)) => Some(url),
            _ => None,
        }
    }

    /// Drop the decoder over a remote source, keeping identity (`media`),
    /// `descriptor`, `capabilities` and `position` so `ensure_source_open`
    /// can reopen it later. A no-op for a local source: nothing about a
    /// local `DecodedSource` is tied to a fetch that a stop, a seek or a
    /// shutdown could have retired, so M1's decoder-survives-a-stop contract
    /// is untouched.
    pub(super) fn retire_remote_source(&mut self) {
        if self.source_is_remote() {
            self.source = None;
        }
    }

    /// The deadline `seek_bounded` gives `SourceInterrupt::
    /// set_operation_deadline` for one seek's underlying reader I/O (§5.3):
    /// the same `limits.stall` figure ordinary reads are bounded to, now
    /// enforced regardless of the freeze level rather than only while
    /// unfrozen (R1's fix). Read fresh on every seek, exactly like
    /// `prepare_context`'s own `limits` below - a service installed after
    /// this worker started is honoured the next time a seek runs, not only
    /// the next time a source opens. Inert for a local seek: nothing local
    /// ever reads `source_interrupt`'s deadline.
    fn seek_deadline(&self) -> Duration {
        lock(&self.http)
            .as_ref()
            .map(|service| service.limits().stall)
            .unwrap_or_default()
    }

    /// The ingredients `prepare` needs, read fresh on every call so a
    /// service installed by `EngineHandle::set_http` after this worker
    /// started is visible the next time a source opens.
    fn prepare_context(&self) -> PrepareContext {
        let http = lock(&self.http).clone();
        // `open_local` never reads `limits`; only `open_http` does, and it
        // has no service to read them from when `http` is `None` - the
        // default is inert in that case, and `prepare` reports the missing
        // service before it would matter anyway.
        let limits = http
            .as_ref()
            .map(|service| *service.limits())
            .unwrap_or_default();
        PrepareContext {
            http,
            interrupt: Arc::clone(&self.source_interrupt),
            hook: Arc::clone(&self.service) as Arc<dyn WaitHook>,
            limits,
            expected: None,
        }
    }

    /// The one place `capabilities` is written, so the handle's out-of-band
    /// view (M7 §6.1) can never disagree with the worker's for longer than
    /// one command.
    fn set_capabilities(&mut self, capabilities: MediaCapabilities) {
        self.capabilities = capabilities;
        self.traits.indefinite.store(
            capabilities.continuity == Continuity::Indefinite,
            Ordering::Release,
        );
        self.traits.seek_unsupported.store(
            capabilities.seek == SeekSupport::Unsupported,
            Ordering::Release,
        );
    }

    pub(super) fn is_indefinite(&self) -> bool {
        self.capabilities.continuity == Continuity::Indefinite
    }

    fn emit_capabilities(&mut self, capabilities: MediaCapabilities) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::CapabilitiesChanged {
            session_rev,
            capabilities,
        });
    }

    /// Reopen a remote source the worker retired, so a caller that needs a
    /// decoder has one. `Ok(false)` means nothing had to be done - `source`
    /// was already open, or nothing has ever been loaded.
    ///
    /// Called by `restore` (play after stop, or after a remote failure), by
    /// `seek_to` (a seek arriving while stopped, or after a previous seek's
    /// retirement dropped the decoder), and by `restart`. Giving it to only
    /// one of them is what makes a stopped seek fail with "nothing is
    /// loaded" on a source that is very much loaded.
    pub(super) fn ensure_source_open(&mut self) -> Result<bool, PlaybackError> {
        if self.source.is_some() {
            return Ok(false);
        }
        let Some(location) = self.descriptor.clone() else {
            return Ok(false);
        };
        // No `begin()` here: `prepare`'s own `HttpMediaSource::open` already
        // opens its own generation as its first action, and a `begin()`
        // taken here first would only be a stale generation number by the
        // time `prepare` returns - `is_current` against it would then be
        // comparing against a generation `open` itself has already moved
        // past, failing every reopen whether anything actually interrupted
        // it or not. Checking `is_retired()` instead is exact: any stop, seek
        // or shutdown landing during `prepare`'s own wait already fails
        // `prepare` itself (its header wait and every probe read answer
        // `Retired` and propagate as an `Err`), so `Ok` here is only ever
        // reached when nothing retired the generation `prepare` opened - the
        // one narrow gap left is a retirement landing in the instant between
        // `prepare` returning and this check, which is the same single-
        // instant race every other cancellation check in this file accepts.
        // M7 §3.5, the symmetric half of `fresh_open`'s guard: an established
        // finite session that reopens to find a station is a changed
        // resource, refused before anything adopts it.
        let mut context = self.prepare_context();
        if self.media.is_some() && self.capabilities.continuity == Continuity::Finite {
            context.expected = Some(Continuity::Finite);
        }
        let mut prepared = prepare(&location, &context)?;
        if self.source_interrupt.is_retired() {
            return Err(PlaybackError::Cancelled);
        }
        // M10: a reopen of the same finite location reads the same container,
        // so a demuxer this session already proved seekable stays proven.
        // Without this every reopen falls back to `Unknown`: an episode would
        // recover from its first outage but never from a second, and a
        // stopped seek would spend a trial seek re-proving what it knew.
        if self.capabilities.seek == SeekSupport::Native
            && prepared.capabilities.continuity == Continuity::Finite
            && prepared.capabilities.seek == SeekSupport::Unknown
        {
            prepared.source.note_demuxer_proven();
            prepared.capabilities = prepared.source.capabilities();
        }
        // A stop-then-play cycle reopens against the
        // same location and answers with the same capabilities every time;
        // only announce when something actually changed.
        if self.capabilities != prepared.capabilities {
            self.emit_capabilities(prepared.capabilities);
        }
        self.set_capabilities(prepared.capabilities);
        self.source = Some(prepared.source);
        // §11: reconnect - this is the one path that reopens a remote source
        // the worker previously retired (a stop, or a failure), rather than
        // establishing one for the first time.
        if let SourceLocation::Http(url) = &location {
            tracing::debug!(url = %redact_url(url.as_str()), "reconnected remote source");
        }
        Ok(true)
    }

    /// One cancellable trial seek to the current position and back, so
    /// `SeekSupport::Unknown` is resolved by demonstration rather than
    /// assumption (§6). Publishes `CapabilitiesChanged` and promotes the
    /// source's own evidence on success, so a later caller reads `Native`
    /// off the same field this one just updated.
    ///
    /// `Result<bool, PlaybackError>`, not a bare `bool`: the trial goes
    /// through `HttpMediaSource::seek`, which issues a real range request,
    /// so a cancellation and a typed remote
    /// failure (a 416, `ResourceChanged`, a stall...) are both reachable here
    /// and neither is the same fact as a demuxer that genuinely refused the
    /// seek. `Ok(false)` is reserved for that last case alone; the caller
    /// tells the three apart rather than reading every failure as "cannot
    /// seek".
    fn verify_seek_support(&mut self) -> Result<bool, PlaybackError> {
        let current = self.position;
        // R2: the other `seek_refined` call site beside `land`, routed through
        // `seek_bounded` so a stopped-seek validation is bounded exactly like
        // an ordinary one - this trial goes over the network precisely
        // because the source has never demonstrated a seek before, so an
        // unbounded rescan here is the same wedge under a different name.
        // Deliberately does not touch `self.position_provenance`: the trial
        // lands back at `current` without ever changing `self.position`, so
        // there is no landing here for provenance to describe.
        let deadline = self.seek_deadline();
        let Some(source) = self.source.as_mut() else {
            return Ok(false);
        };
        let outcome = seek_bounded(
            source,
            &self.source_interrupt,
            &self.interrupt,
            deadline,
            current,
            None,
        );
        match outcome {
            Ok(_) => {
                if let Some(source) = self.source.as_mut() {
                    source.note_demuxer_proven();
                    // Only reachable with `self.capabilities.seek == Unknown`
                    // (this method's one caller gates on exactly that), so a
                    // successful trial is always a change to `Native` -
                    // nothing to compare here.
                    let capabilities = source.capabilities();
                    self.set_capabilities(capabilities);
                    self.emit_capabilities(capabilities);
                }
                Ok(true)
            }
            // Cancelled or a typed remote failure: propagate rather than
            // flatten into "cannot seek" (see this method's own doc comment).
            Err(error) if is_cancelled(&error) || remote_cause(&error).is_some() => Err(error),
            // Neither cancelled nor remote: the demuxer itself refused.
            Err(_) => Ok(false),
        }
    }
}

/// True when this error is a retirement rather than a fault: a stop, a seek
/// or a shutdown retired the read that was in flight, and the decoder is now
/// at an arbitrary point rather than having reached EOF or a genuine
/// failure. §8's whole read-outcome classification rests on this being
/// right — the foundation everything else in this task builds on.
///
/// Only `Decode` and a direct `Remote(Cancelled)` are checked: `next_planar`
/// only ever produces `PlaybackError::Decode`, so the second arm is
/// defensive rather than reachable from `pump_audio` today. `SeekFailed` -
/// the shape a cancelled *seek* takes - is deliberately not covered here;
/// that is `is_cancelled`'s job, which walks `remote_cause` generically
/// rather than matching a fixed set of variants, since a seek's cancellation
/// can arrive wrapped in either `Decode` (a seek's own trial reads) or
/// `SeekFailed` (the reader's own `seek()` call).
fn is_retired_read(error: &PlaybackError) -> bool {
    match error {
        PlaybackError::Decode(inner) => is_retired(inner),
        PlaybackError::Remote(RemoteFailure::Cancelled) => true,
        _ => false,
    }
}

/// A cancelled operation is not a failure. Local cancellation sets
/// `PlaybackError::Cancelled` directly, through the `self.interrupt` flag
/// every refinement loop checks. Remote cancellation instead runs all the
/// way down through a retired `SourceInterrupt` and back up wrapped in
/// whatever `PlaybackError` variant the failing call produces (`Decode` from
/// a read, `SeekFailed` from the reader's own `seek()`) - `remote_cause`
/// walks the `source()` chain regardless of which, so this recognises both
/// without matching either shape by name. Either way the interrupt that
/// cancelled the operation is still set, so the loop's next pass turns it
/// into the stop, seek or shutdown the user actually asked for, which is why
/// every caller of this function treats the two identically.
pub(super) fn is_cancelled(error: &PlaybackError) -> bool {
    matches!(error, PlaybackError::Cancelled)
        // A direct `PlaybackError::Remote(RemoteFailure::Cancelled)` - what
        // `prepare`/`HttpMediaSource::open` and `ensure_source_open` produce,
        // never routed through Symphonia at all - is not caught by
        // `remote_cause` below: `#[error(transparent)]` forwards `source()`
        // to the inner `RemoteFailure`'s own `source()`, and `Cancelled` has
        // none, so walking the chain finds nothing. `remote_cause` handles
        // the *decode-layer* case, where Symphonia has wrapped the failure
        // in a `RemoteIoError`.
        || matches!(error, PlaybackError::Remote(RemoteFailure::Cancelled))
        || matches!(remote_cause(error), Some(RemoteFailure::Cancelled))
}

/// Whether the interrupt word carries a stop or a shutdown. Every local
/// refinement loop's `cancelled` closure checks this rather than `!= 0`: the
/// word exists to wake the worker between passes, and a bare `!= 0` would
/// read any future out-of-band bit as "abandon the operation in progress."
/// STOP and SHUTDOWN mean exactly that - the operation in progress must give
/// up regardless of what it is.
fn stop_or_shutdown(word: u8) -> bool {
    word & (STOP | SHUTDOWN) != 0
}

/// The one seat every `seek_refined` call goes through (R2 of M3.1 Task 4):
/// `Worker::land` (every landing: load, seek, restore, restart, rebuild and
/// recovery) and `verify_seek_support` call this rather than
/// `DecodedSource::seek_refined` directly, so the deadline policy below
/// cannot drift between them.
///
/// A free function rather than a `Worker` method, so its callers keep the
/// disjoint `self.source`/`self.source_interrupt`/`self.interrupt` borrows.
///
/// Sets a fresh, operation-scoped deadline on `source_interrupt` for the
/// duration of `seek_refined`'s own `reader.seek()` call and clears it back
/// to `None` once that call returns, success or failure alike (§5.3) - one
/// deadline, applied identically whether `source` is remote or local (inert
/// for a local source, which never touches `source_interrupt`'s deadline at
/// all) and identically across every caller, per the brief's rule against
/// special-casing by transport. `deadline` is computed fresh by every caller
/// from `Worker::seek_deadline()` immediately before this runs, which is
/// what makes a recovery attempt's deadline a *fresh* one (§5.4) rather than
/// an inherited, possibly-already-expired one: nothing here remembers a
/// deadline across calls.
///
/// `SeekMode::Coarse` is what `seek_refined` performs now (`decode.rs`), but
/// that mode swap only changes *behaviour* for MP3 - it is the only
/// `FormatReader::seek` in this crate's dependency tree that reads `mode` at
/// all. So the provenance this returns is **not** asserted
/// here: it is read straight off `SeekOutcome::provenance`, which
/// `seek_refined` itself computes from the reader's own `format_info()` -
/// `Estimated` for MP3, `Established` for every other format this crate
/// supports, exactly as it always reported before this task, never inferred
/// from the target, a Xing/Info tag, or which caller reached it. A landing
/// already at its target does not call this at all (`land`'s no-op), so it
/// never manufactures a landing that never happened.
fn seek_bounded(
    source: &mut DecodedSource,
    source_interrupt: &SourceInterrupt,
    local_interrupt: &AtomicU8,
    deadline: Duration,
    target: Duration,
    budget: Option<Duration>,
) -> Result<(SeekOutcome, PositionProvenance), PlaybackError> {
    source_interrupt.set_operation_deadline(Some(source_interrupt.now() + deadline));
    let outcome = source.seek_refined(target, budget, &mut || {
        stop_or_shutdown(local_interrupt.load(Ordering::Acquire))
    });
    source_interrupt.set_operation_deadline(None);
    // Provenance comes from `SeekOutcome` itself, not asserted here: it
    // follows the demuxer that actually ran `Coarse` (MP3 only),
    // never asserted uniformly for every landing regardless of format.
    outcome.map(|outcome| {
        let provenance = outcome.provenance;
        (outcome, provenance)
    })
}

pub(super) fn frames_to_duration(frames: u64, rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(rate.max(1)))
}

/// Best-effort text for a `std::thread::JoinHandle::join` panic payload. Panic
/// payloads are almost always a `&'static str` or a `String`; anything else
/// reports as opaque rather than being dropped silently.
pub(super) fn describe_panic(panic: &(dyn Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Keep the promised value when the difference is sub-frame quantization, in
/// either direction, and adopt the decoder's answer when it is a real
/// difference. Dropping this makes both
/// `play_from_stopped_resumes_at_the_preserved_position_without_resetting`
/// and `transport_recreation_preserves_position` fail on their `>=`.
fn adopt_preserved(promised: Duration, actual: Duration) -> Duration {
    if actual.abs_diff(promised) <= RESUME_TOLERANCE {
        promised
    } else {
        actual
    }
}

/// An estimated duration is not a ceiling (§5.5): `clamp_target` must treat
/// it exactly like an absent one, the same shape `decide_resume` uses for
/// `KnownDuration`. Clamping to an estimate would silently relocate a seek
/// the listener asked for; better to attempt it and let symphonia's own
/// `max_ts` check refuse it honestly if it is genuinely out of range (that
/// check precedes `SeekMode` dispatch and so applies identically either
/// way — this does **not** make an under-estimated file's tail reachable,
/// and that limitation is retained deliberately).
///
/// Factored out of `clamp_target` so this branch is provable in isolation:
/// no integration test drives it through a live `Worker` today (only this
/// module's own unit test holds it), even though `DecodedSource` has
/// reported `Estimated` from real Xing/VBRI detection since Task 9.
fn established_duration(metadata: &MediaMetadata) -> Option<Duration> {
    (metadata.duration_provenance == PositionProvenance::Established)
        .then_some(metadata.duration)
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::DIAGNOSTIC_INTERVAL;
    use super::super::diagnostics::retire_faults;
    use crate::playback::wait::Servicing;
    use super::*;
    use crate::playback::outbox;
    use crate::playback::output::null_output::NullOutput;
    use crate::playback::spectrum::worker as spectrum_worker;
    use crate::playback::timeline::PositionQuality;

    /// A worker over `NullOutput` whose loop never runs, the progress slot
    /// its hook publishes to, and the receiver its events reach. The spectrum
    /// thread is stopped at once: the worker only needs the port.
    fn idle_worker() -> (
        Worker,
        Arc<Mutex<Progress>>,
        Receiver<PlaybackEvent>,
        crossbeam_channel::Sender<OutputFault>,
    ) {
        let (events_out, events) = outbox::channel();
        let progress = Arc::new(Mutex::new(Progress {
            session_rev: 0,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
            load: None,
        }));
        let (faults_tx, faults) = crossbeam_channel::unbounded();
        let (_commands_tx, commands) = crossbeam_channel::bounded(1);
        let (liveness, _liveness_rx) = crossbeam_channel::bounded(0);
        let (_wake_tx, wake) = crossbeam_channel::bounded(1);
        let device_clock = Arc::new(AtomicU64::new(0));
        let (_spectrum, port, mut thread) = spectrum_worker::spawn(Arc::clone(&device_clock));
        thread.stop();
        let worker = Worker::new(
            Box::new(NullOutput::new()),
            faults,
            Arc::clone(&progress),
            commands,
            events_out,
            liveness,
            wake,
            Arc::new(AtomicU8::new(0)),
            SourceInterrupt::new(1024),
            Arc::new(SourceTraits::default()),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(ReconnectPolicy::default())),
            device_clock,
            port,
        );
        (worker, progress, events, faults_tx)
    }

    /// Outbox spec §4.3. A read that blocks after a command emitted must not
    /// deliver that event against facts from before the command: here a
    /// stored seek's `SeekCompleted` would reach the application with the
    /// stopped position still published under the same revision, and the
    /// session would checkpoint that position over the seek.
    #[test]
    fn an_event_flushed_by_a_blocked_read_carries_progress_from_its_emission() {
        let (mut worker, progress, events, _faults_tx) = idle_worker();
        worker.state = PlaybackState::Stopped;
        worker.session_rev = 7;
        worker.position = Duration::from_secs(1);
        worker.publish_progress();

        // As `restore` leaves the worker: same revision, landed and playing,
        // with no loop pass since.
        worker.state = PlaybackState::Playing;
        worker.position = Duration::from_secs(3);
        worker.position_provenance = PositionProvenance::Established;
        worker.emit(PlaybackEvent::SeekCompleted {
            session_rev: 7,
            requested: Duration::from_secs(3),
            actual: Duration::from_secs(3),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        });

        // What a read blocking in step 7 runs.
        worker.service.service_as(Servicing::BlockedRead);

        assert!(
            matches!(
                events.try_recv(),
                Ok(PlaybackEvent::SeekCompleted { session_rev: 7, .. })
            ),
            "the blocked read did not deliver the event"
        );
        let published = lock(&progress).clone();
        assert_eq!(published.session_rev, 7);
        assert_eq!(published.position, Duration::from_secs(3));
    }

    /// Review focus 3: a warning `offer` refuses is not counted as reported,
    /// so the next interval offers the same totals again.
    #[test]
    fn a_refused_warning_is_offered_again_at_the_next_interval() {
        let (mut worker, _progress, events, _faults_tx) = idle_worker();
        // Fill the channel to its ordinary limit (64 - 9 reserved = 55).
        for n in 0..55 {
            worker.outbox.emit(PlaybackEvent::Warning {
                session_rev: 0,
                message: n.to_string(),
            });
        }
        worker.outbox.flush();
        worker.glitches.xruns = 1;
        let long_ago = Instant::now().checked_sub(DIAGNOSTIC_INTERVAL).unwrap();
        worker.glitches.last_report = long_ago;

        worker.emit_aggregated_warning_if_slot_free();
        assert_eq!(
            worker.glitches.reported_total, 0,
            "a refused warning counted as reported"
        );
        assert_eq!(events.len(), 55);

        let _ = events.try_recv();
        worker.emit_aggregated_warning_if_slot_free();
        assert_eq!(worker.glitches.reported_total, 1);
        let last = events.try_iter().last();
        assert!(
            matches!(&last, Some(PlaybackEvent::Warning { message, .. }) if message.starts_with("audio glitches: 1 underruns")),
            "{last:?}"
        );
    }

    /// Fix round 2: proves the exact `store`/`fetch_max` composition
    /// `open_transport`, `CallbackCore::fill` and `Worker::publish_progress`
    /// share for `device_clock`, in isolation.
    ///
    /// This is a mechanism-level test, not an integration one, and
    /// deliberately so: driving it through a real `Worker` would need
    /// `self.output.now()` to actually go backward across a rebuild — the
    /// real symptom a missed reset produces — but `TestOutput`'s virtual
    /// clock is one persistent, monotonically-advancing counter that `open`
    /// never resets, unlike a real `cpal::Stream`'s `StreamInstant`, which
    /// restarts near zero for every new stream. Every other position-
    /// preservation test in this crate (`transport_recreation_preserves_position`
    /// among them) depends on that exact continuity to assert exact values
    /// across a rebuild, so making `TestOutput` model a per-stream domain
    /// reset to reach this case here would risk destabilizing tests this
    /// crate protects rather than proving the one this test exists for.
    /// What is left provable this way is the mechanism itself: that a plain
    /// `store` genuinely un-sticks a value `fetch_max` climbed, which is the
    /// whole reason `open_transport` uses one rather than a `fetch_max(0,
    /// ..)` that would be a silent no-op against any nonzero high-water mark.
    #[test]
    fn a_domain_reset_uses_a_plain_store_so_fetch_max_does_not_latch_the_old_high_water_mark() {
        let device_clock = AtomicU64::new(0);

        // The old domain climbs, exactly as the two steady-state `fetch_max`
        // writers (`CallbackCore::fill`, `Worker::publish_progress`) do.
        device_clock.fetch_max(1_000_000_000, Ordering::Relaxed); // 1s
        device_clock.fetch_max(2_000_000_000, Ordering::Relaxed); // 2s
        assert_eq!(device_clock.load(Ordering::Relaxed), 2_000_000_000);

        // `open_transport` resets for the new domain with a plain `store`.
        device_clock.store(0, Ordering::Relaxed);
        assert_eq!(
            device_clock.load(Ordering::Relaxed),
            0,
            "a plain store must win over the old domain's high-water mark"
        );

        // The new domain's own steady-state writers resume with `fetch_max`,
        // reporting values far below the old domain's 2s mark - exactly what
        // a freshly opened stream's `StreamInstant`s do.
        device_clock.fetch_max(50_000_000, Ordering::Relaxed); // 50ms into the new stream
        assert_eq!(
            device_clock.load(Ordering::Relaxed),
            50_000_000,
            "the new domain's own values must be honoured, not clamped by the old domain"
        );
        // The counterfactual this guards: had the reset itself used
        // `fetch_max(0, ..)` instead of `store`, it would have been a no-op
        // against the 2s mark above, and the final assertion would instead
        // see `device_clock` still reporting 2_000_000_000 - a position
        // frozen at the old stream's last instant for the entire life of the
        // new one, which is the exact regression Part 2 of this fix exists
        // to prevent.
    }

    #[test]
    fn retiring_faults_clears_the_deferred_slot_and_the_queue() {
        // Both halves matter. Clearing only the deferred slot leaves a fault
        // that was still queued when the interrupt arrived, and that one fires
        // after the stop completes.
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(OutputFault::Fatal(cpal::ErrorKind::PermissionDenied))
            .expect("queue accepts a fault");
        tx.send(OutputFault::Rebuild(cpal::ErrorKind::DeviceNotAvailable))
            .expect("queue accepts a fault");
        let mut deferred = Some(OutputFault::Fatal(cpal::ErrorKind::HostUnavailable));

        retire_faults(&mut deferred, &rx);

        assert!(
            deferred.is_none(),
            "a deferred fault outlived its transport"
        );
        assert!(
            rx.try_recv().is_err(),
            "a queued fault outlived its transport"
        );
    }

    #[test]
    fn retiring_faults_is_safe_when_there_is_nothing_to_retire() {
        let (_tx, rx) = crossbeam_channel::bounded::<OutputFault>(8);
        let mut deferred = None;
        retire_faults(&mut deferred, &rx);
        assert!(deferred.is_none());
    }

    // ---------------------------------------------------- established_duration
    //
    // `clamp_target`'s Estimated branch is not driven through a live
    // `Worker` by any integration test today, even though `DecodedSource`
    // has reported real `Estimated` durations from Xing/VBRI detection
    // since Task 9 — no test happens to combine a live worker with a
    // headerless MP3 and a seek past its estimated ceiling. These two tests
    // exercise the extracted decision directly instead, against a bare
    // `MediaMetadata` literal, which needs no decoder at all. Together they
    // are the two-sided proof the plan calls for: an implementation that
    // simply deleted the clamp would fail the second one, and one that never
    // implemented the skip at all would fail the first.

    #[test]
    fn an_estimated_duration_is_not_treated_as_a_ceiling() {
        let metadata = MediaMetadata {
            title: None,
            artist: None,
            album: None,
            year: None,
            duration: Some(Duration::from_secs(100)),
            duration_provenance: PositionProvenance::Estimated,
            front_cover: None,
        };
        assert_eq!(
            established_duration(&metadata),
            None,
            "an estimated duration must not be usable as a clamp ceiling"
        );
    }

    #[test]
    fn an_established_duration_is_still_a_ceiling() {
        let metadata = MediaMetadata {
            title: None,
            artist: None,
            album: None,
            year: None,
            duration: Some(Duration::from_secs(100)),
            duration_provenance: PositionProvenance::Established,
            front_cover: None,
        };
        assert_eq!(
            established_duration(&metadata),
            Some(Duration::from_secs(100)),
            "an established duration is the M1/M2 ceiling, unchanged"
        );
    }

    /// The worker-side generation is u32: 65,536 bumps — a held key-repeat's
    /// worth of seeks, or one reopen per outage across a long session — must
    /// not wrap back onto a mark recorded a lifetime ago. With the u16 this
    /// replaced, exactly this many bumps landed the generation back on the
    /// seeded mark's value, and `settle_heard`'s match arm would credit the
    /// whole gap as newly heard. The wire's 16-bit truncation is the
    /// handshake/link suites' concern, not this one's.
    #[test]
    fn a_generation_does_not_wrap_onto_a_live_mark_after_65_536_bumps() {
        let (mut worker, _progress, _events, _faults_tx) = idle_worker();
        worker.generation = 1;
        worker.heard_mark = Some((1, Duration::from_millis(500)));

        for _ in 0..65_536 {
            worker.reset_generation_state();
        }

        assert_eq!(worker.generation, 65_537);
        assert_ne!(
            worker.heard_mark.map(|(marked, _)| marked),
            Some(worker.generation),
            "a mark from generation 1 must not match after 65,536 bumps"
        );
    }

    /// An underrun counts no frames: the timeline freezes at what was really
    /// heard, which stays exact, so a benign `Xrun` must not latch degraded
    /// quality onto the whole generation. A rerouted device or denied
    /// realtime scheduling still does — their timing base can jump.
    #[test]
    fn an_xrun_does_not_latch_degraded_quality() {
        let (mut worker, _progress, _events, faults_tx) = idle_worker();
        worker.degraded = false;

        faults_tx
            .send(OutputFault::Recoverable(cpal::ErrorKind::Xrun))
            .expect("faults queue accepts an Xrun");
        worker.service_faults();
        assert!(
            !worker.degraded,
            "an underrun must not degrade the position quality"
        );

        faults_tx
            .send(OutputFault::Recoverable(cpal::ErrorKind::DeviceChanged))
            .expect("faults queue accepts a DeviceChanged");
        worker.service_faults();
        assert!(
            worker.degraded,
            "a rerouted device still degrades the position quality"
        );
    }
}
