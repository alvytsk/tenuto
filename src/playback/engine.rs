//! The decode worker: the state machine that owns every pipeline component.
//!
//! The invariant this module exists to enforce is that stopping or recreating
//! the audio pipeline never implicitly resets the logical playback position.
//! Position lives here, in `Worker::position`, and only four things establish a
//! new one: loading media, an explicit restart, a successful seek, and a resume
//! that adopts a target stored while stopped. Everything else — pausing,
//! stopping, a device disappearing, a handshake timing out — preserves it.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Sender, TrySendError, select};
use url::Url;

use crate::http::channel::{ByteChannel, ReadOutcome, SourceInterrupt, WaitHook};
use crate::http::error::{Operation, RemoteFailure, redact_url};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::{is_retired, remote_cause};
use crate::media::capabilities::{Continuity, MediaCapabilities, ResumeCapability, SeekSupport};
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;
use crate::media::source::SourceLocation;
use crate::resume::{KnownDuration, ResumeDecision, decide_resume};

use super::callback::CallbackCore;
use super::command::{Admission, LoadRequestId, PlaybackCommand, ResumeIntent};
use super::decode::{DecodedSource, SeekOutcome};
use super::error::PlaybackError;
use super::event::{PlaybackEvent, Progress, ShutdownReport, StartDisposition};
use super::handshake::Handshake;
use super::link::OutputLink;
use super::output::cpal_output::{CpalOutput, OutputFault};
use super::output::null_output::NullOutput;
use super::output::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest, SpanRecord};
use super::prepare::{PrepareContext, prepare};
use super::provenance::PositionProvenance;
use super::reconnect::{Next, Outage, ReconnectPolicy, retryable};
use super::resample::Converter;
use super::spectrum::registry::{TapMapping, TapRegistry};
use super::spectrum::worker::{
    self as spectrum_worker, SpectrumHandle, SpectrumPort, SpectrumThread,
};
use super::state::PlaybackState;
use super::timeline::{PositionQuality, Timeline};
use super::volume::Volume;
use super::wait::{Servicing, SessionFacts, WaitService};

const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;
/// A seek whose target has already been accepted by the command queue. §8:
/// the interrupt is published only after acceptance, so a seek that was
/// refused admission cannot retire a fetch it never got to replace.
const SEEK: u8 = 4;
const TICK: Duration = Duration::from_millis(10);
/// `pub(crate)` so `wait.rs`'s `WaitService` — the hook's other caller — waits
/// on the exact same deadline as the worker's own handshake calls.
pub(crate) const DEADLINE: Duration = Duration::from_millis(250);
/// Idle wait inside a handshake. The device runs on its own thread, so the
/// worker only has to stop spinning while it waits for an acknowledgment.
/// `pub(crate)`, shared with `wait.rs`, so the hook's `park`/`release` calls
/// pump the wait exactly as the worker's own do.
pub(crate) const PUMP_NAP: Duration = Duration::from_micros(250);
/// Ordinary events may not occupy these; terminal outcomes may.
///
/// `pub(crate)` (Ruling 7) so `wait.rs`'s `announce` — the hook's own event
/// path — respects the same reserve `flush_events` does; the hook is a second
/// emitter and must never occupy the reserved tail.
pub(crate) const RESERVED_EVENT_SLOTS: usize = 9;
pub(crate) const EVENT_CAPACITY: usize = 64;
const PENDING_CAP: usize = 128;

// Reserve budget. Terminal or protected outcomes may occupy the reserved
// tail; ordinary events may not. The worst case is one loop iteration
// emitting, at most:
//
//   stop interrupt        1  StateChanged{Stopped}
//   a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
//   a dispatched command  5  Load is the widest: StateChanged{Loading},
//                            CapabilitiesChanged, Loaded, then either
//                            StateChanged{Paused} (`open_transport` succeeds)
//                            or Failed + StateChanged{Failed} (it does not) -
//                            the failure tail is one event wider than the
//                            success one, so 5 is this row's true worst case
//                            (fix round 2, MINOR: this used to read 4).
//   end of track          2  EndOfTrack + StateChanged{Ended}
//                        --
//                        10 (naive union)
//
// 10 looks like it breaks `RESERVED_EVENT_SLOTS == 9`, but the reserve only
// has to be as large as the TERMINAL-OR-PROTECTED share of that union: an
// ordinary event that cannot flush is merely held in `pending_events`
// (bounded separately, by `PENDING_CAP`) until the reserve clears - never
// lost, and so never in need of reservation. Only a terminal or protected
// outcome, which the reserved tail exists to guarantee delivery for even
// under a full ordinary backlog, must actually fit (Decision 4, Decision 5).
// Row by row:
//
//   stop interrupt        1  StateChanged{Stopped} (terminal)
//   a fatal fault          2  Failed + StateChanged{Failed} (both terminal)
//   a dispatched load      3  Loaded (protected, M5 §6) + Failed +
//                             StateChanged{Failed} (both terminal) - `Loaded`
//                             itself is not terminal but must still fit, so
//                             this row's share is now one wider than a
//                             fault's.
//   end of track           2  EndOfTrack + StateChanged{Ended} (both terminal)
//                        --
//                         8 <= RESERVED_EVENT_SLOTS (9)
//
// A cancelled load - a stop or shutdown landing during the open - emits only
// `LoadCancelled` (1, protected), well under the dispatched-load row's own
// share, so it never raises the bound. These four rows are still not
// mutually exclusive in a single pass, so the union is the bound rather than
// the maximum of them; command admission closes while a backlog exists, and
// `service_faults` defers a fault whose events would not fit, so neither
// source can outrun the drain.
//
// `RestartEstablished` and `SeekCancelled` both belong to commands narrower
// than `Load` (their own event plus a `StateChanged`, at most 2, and neither
// terminal), so neither raises the bound. The resume-unavailable warning does
// not add a sixth to the dispatched-command row either: it rides on
// `Loaded.disposition` rather than an event of its own.
const COMMAND_CAPACITY: usize = 1024;
const SPAN_CAPACITY: usize = 64;
/// How much audio the PCM ring holds. Large enough that one loop iteration
/// cannot drain it, small enough that discarding it on a seek is cheap.
const RING_MILLIS: u64 = 300;
/// Floor on the interval between aggregated diagnostic warnings.
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_millis(500);
/// Refinement budget for an *explicit* seek. Preserving seeks pass `None`.
const SEEK_BUDGET: Duration = Duration::from_secs(5);
/// A refined seek lands on a whole *source* frame, while the position it is
/// asked to preserve is a whole *output* frame, so a preserving seek can come
/// back a fraction of a frame short of what it promised. Within this tolerance
/// the promise is kept, so that repeated stop/resume cycles cannot walk the
/// position backwards a frame at a time. Pause and resume no longer need this -
/// they never re-seek - but stop, resume-at-a-stored-target and device recovery
/// still do.
///
/// One source frame at 44.1 kHz is 22.68 us, and the residues this actually
/// absorbs measure 4.5 us and 13.6 us. 100 us is four of those frames: wide
/// enough for the quantum, and an order of magnitude below the packet-sized
/// miss a genuinely failed seek leaves, so a real miss surfaces rather than
/// being swallowed.
const RESUME_TOLERANCE: Duration = Duration::from_micros(100);

/// The event stream, handed out as one unit.
///
/// The liveness receiver never carries a message. It exists so that dropping
/// the stream is *observable*: a rendezvous send is ready only when the channel
/// is disconnected, so the worker learns from the channel's own error that
/// nobody is listening. Occupancy is never consulted for that question.
pub struct EventStream {
    events: Receiver<PlaybackEvent>,
    _liveness: Receiver<()>,
}

impl EventStream {
    fn disconnected() -> Self {
        let (_events_tx, events) = crossbeam_channel::bounded(1);
        let (_liveness_tx, liveness) = crossbeam_channel::bounded(0);
        Self {
            events,
            _liveness: liveness,
        }
    }
}

/// What the handle is allowed to assume about the open source before the
/// worker has looked at the command it is submitting (M7 §6.1).
///
/// A cache of two bits of `Worker::capabilities`, published by
/// `Worker::set_capabilities` — the one place that field is written — so the
/// two views cannot disagree for longer than the single command in flight.
/// Wrong for that instant costs nothing either way: a seek the handle thought
/// was seekable is still rejected by the worker, and one it thought was not
/// simply reaches the worker without the retirement it did not need.
#[derive(Debug, Default)]
pub(crate) struct SourceTraits {
    pub indefinite: AtomicBool,
    pub seek_unsupported: AtomicBool,
}

pub struct EngineHandle {
    commands: Sender<PlaybackCommand>,
    stream: EventStream,
    progress: Arc<Mutex<Progress>>,
    interrupt: Arc<AtomicU8>,
    wake: Sender<()>,
    worker: Option<JoinHandle<Vec<PlaybackEvent>>>,
    /// The one `SourceInterrupt` this worker's whole life uses, shared with
    /// every `HttpMediaSource` it ever opens (Carried Finding 2's "construct
    /// a real one and replace the placeholder" - there is only ever one, so
    /// there is nothing to swap). `submit_seek`/`submit_pause`/`submit_play`/
    /// `submit_stop`/`submit_shutdown` act on it directly, out of band from
    /// the command queue, so a blocked read is reachable without waiting for
    /// the worker to drain its backlog.
    source_interrupt: Arc<SourceInterrupt>,
    /// The worker's published view of the open source, read by `submit_seek`
    /// to decide whether retiring the source is worth anything (M7 §6.1).
    traits: Arc<SourceTraits>,
    /// `None` until `set_http` installs one. Shared with the worker so
    /// `Worker::load` can build a `PrepareContext` from whatever is
    /// installed at the moment it runs.
    http: Arc<Mutex<Option<Arc<HttpService>>>>,
    /// Shared with the worker, so a policy installed after the worker was
    /// spawned governs the very next outage (M7 §7).
    reconnect_policy: Arc<Mutex<ReconnectPolicy>>,
    /// The analysis worker's application side (§10), handed out by
    /// [`Self::spectrum`].
    spectrum: SpectrumHandle,
    /// The `tenuto-spectrum` thread: stopped and joined by [`Self::join`]
    /// and by `Drop`, so it never outlives the engine.
    spectrum_thread: SpectrumThread,
}

impl EngineHandle {
    /// Spawn a worker over an output that reports no asynchronous faults.
    pub fn spawn(output: Box<dyn AudioOutput>) -> Self {
        Self::spawn_with(output, crossbeam_channel::never())
    }

    /// Spawn a worker over the default cpal device, wired for fault recovery.
    pub fn spawn_cpal() -> Self {
        let mut output = CpalOutput::default();
        let faults = output.faults();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(64);
        output.set_wake(wake_tx.clone());
        Self::assemble(Box::new(output), faults, wake_tx, wake_rx)
    }

    /// Spawn over whichever output the environment calls for: the paced,
    /// deviceless [`NullOutput`] when `TENUTO_AUDIO_OUTPUT=null` (what a
    /// subprocess test sets on a CI machine with no sound device), otherwise
    /// the real default device.
    pub fn spawn_for_environment() -> Self {
        if std::env::var("TENUTO_AUDIO_OUTPUT").as_deref() == Ok("null") {
            Self::spawn(Box::new(NullOutput::new()))
        } else {
            Self::spawn_cpal()
        }
    }

    /// Spawn a worker over any output, plus the fault stream it publishes to.
    pub fn spawn_with(output: Box<dyn AudioOutput>, faults: Receiver<OutputFault>) -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(64);
        Self::assemble(output, faults, wake_tx, wake_rx)
    }

    fn assemble(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        wake_tx: Sender<()>,
        wake_rx: Receiver<()>,
    ) -> Self {
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (events_tx, events_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let (liveness_tx, liveness_rx) = crossbeam_channel::bounded(0);
        let progress = Arc::new(Mutex::new(Progress {
            session_rev: 0,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
            load: None,
        }));
        let interrupt = Arc::new(AtomicU8::new(0));
        // One real capacity for the worker's whole life (Carried Finding 2):
        // every `HttpMediaSource` this session ever opens is handed the same
        // `Arc`, so its buffer has to be sized for real fetch throughput from
        // the moment it exists, not the byte-at-a-time placeholder that used
        // to sit here only so `WaitService` had something to poll.
        let source_interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
        let traits = Arc::new(SourceTraits::default());
        let http = Arc::new(Mutex::new(None));
        let reconnect_policy = Arc::new(Mutex::new(ReconnectPolicy::default()));
        // Shared by the callback (writer), the decode worker, `WaitService`
        // and the spectrum worker, which schedules frames against it.
        let device_clock = Arc::new(AtomicU64::new(0));
        let (spectrum, spectrum_port, spectrum_thread) =
            spectrum_worker::spawn(Arc::clone(&device_clock));
        let worker = Worker::new(
            output,
            faults,
            Arc::clone(&progress),
            commands_rx,
            events_tx,
            liveness_tx,
            wake_rx,
            Arc::clone(&interrupt),
            Arc::clone(&source_interrupt),
            Arc::clone(&traits),
            Arc::clone(&http),
            Arc::clone(&reconnect_policy),
            device_clock,
            spectrum_port,
        );
        let join = std::thread::Builder::new()
            .name("tenuto-decode".into())
            .spawn(move || worker.run())
            .ok();
        Self {
            commands: commands_tx,
            stream: EventStream {
                events: events_rx,
                _liveness: liveness_rx,
            },
            progress,
            interrupt,
            wake: wake_tx,
            worker: join,
            source_interrupt,
            traits,
            http,
            reconnect_policy,
            spectrum,
            spectrum_thread,
        }
    }

    /// The spectrum analysis worker: enable it, read its latest frame.
    /// Analysis starts disabled.
    pub fn spectrum(&self) -> SpectrumHandle {
        self.spectrum.clone()
    }

    pub fn commands(&self) -> &Sender<PlaybackCommand> {
        &self.commands
    }

    pub fn events(&self) -> &Receiver<PlaybackEvent> {
        &self.stream.events
    }

    /// The wake channel: a device backend pings it so a fault interrupts a
    /// blocked wait rather than waiting out the tick.
    pub fn wake(&self) -> &Sender<()> {
        &self.wake
    }

    /// Release the event stream. The worker sees the disconnection and shuts
    /// down: an engine nobody listens to has no reason to hold a device open.
    pub fn release_events(&mut self) {
        self.stream = EventStream::disconnected();
        let _ = self.wake.try_send(());
    }

    pub fn progress(&self) -> Progress {
        match self.progress.lock() {
            Ok(progress) => progress.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Out-of-band stop: travels on the interrupt flag plus the wake channel,
    /// so a saturated event channel cannot delay it. Also retires the source
    /// interrupt: a stop must reach a worker blocked inside a remote read,
    /// not only one waiting on the command channel or the tick.
    pub fn interrupt_stop(&self) {
        // §11: cancellation, logged at the point the application actually
        // decided on one - not inside `SourceInterrupt::retire` itself, which
        // also runs on every ordinary remote-failure exit path and would
        // mislabel a genuine fault as a user-requested cancellation.
        tracing::debug!("stop requested; retiring the in-flight source read");
        self.source_interrupt.retire();
        self.interrupt.fetch_or(STOP, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    /// Out-of-band shutdown. `Drop` routes through here, so retiring the
    /// source interrupt is what keeps a dropped `EngineHandle` from leaving
    /// the worker thread blocked forever inside a remote read nobody will
    /// ever answer.
    pub fn interrupt_shutdown(&self) {
        // §11: cancellation - see `interrupt_stop`'s comment for why this is
        // logged here rather than inside `retire` itself.
        tracing::debug!("shutdown requested; retiring the in-flight source read");
        self.source_interrupt.retire();
        self.interrupt.fetch_or(SHUTDOWN, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    fn try_send(&self, command: PlaybackCommand) -> Admission {
        match self.commands.try_send(command) {
            Ok(()) => Admission::Accepted,
            Err(TrySendError::Full(_)) => Admission::Busy,
            Err(TrySendError::Disconnected(_)) => Admission::Gone,
        }
    }

    /// Every submission that must be able to reach a worker blocked in a
    /// source read. §8: queue admission and waking belong together, so a
    /// caller cannot queue a command and forget to wake anything.
    pub fn submit(&self, command: PlaybackCommand) -> Admission {
        // A load tears the current source down whatever it is, so its blocked
        // read or open is woken now rather than waited out (M7 §6.1). Aimed at
        // the generation observed *before* the send, like `submit_seek`.
        let replaced = matches!(command, PlaybackCommand::Load { .. })
            .then(|| self.source_interrupt.generation());
        let admission = self.try_send(command);
        if admission == Admission::Accepted {
            if let Some(generation) = replaced {
                self.source_interrupt.retire_generation(generation);
            }
            let _ = self.wake.try_send(());
        }
        admission
    }

    /// `SeekTo(target)`, published to the source interrupt only after the
    /// queue accepts it (§8): a seek refused admission must not retire a
    /// fetch it never got to replace.
    ///
    /// The retirement is aimed at the generation that was live *before* the
    /// send, never at whatever is current afterwards. Between the send and
    /// the retirement this thread can be preempted, and the worker - woken
    /// by the send itself - can dispatch the seek and `begin()` the seek's
    /// own generation in that gap; an unaimed retirement landing then kills
    /// the reopen or trial the seek just opened and misreports the seek as
    /// cancelled (CI on main, 2026-09-17, three different tests, all
    /// `SeekCancelled` with no stop in sight). A superseded generation means
    /// the worker is no longer blocked in anything this had to wake, so the
    /// aimed retirement doing nothing is the correct outcome, not a missed
    /// one.
    pub fn submit_seek(&self, target: Duration) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(PlaybackCommand::SeekTo(target));
        if admission != Admission::Accepted {
            return admission;
        }
        // A source that cannot seek is going to answer `SeekRejected`; retiring
        // it first would cost the listener the stream for nothing (M7 §3.4).
        if !self.traits.seek_unsupported.load(Ordering::Acquire) {
            self.source_interrupt.retire_generation(generation);
            self.interrupt.fetch_or(SEEK, Ordering::Release);
        }
        let _ = self.wake.try_send(());
        admission
    }

    /// `Pause`, plus the freeze level that reaches a read already blocked
    /// inside the byte channel (G2): a bit cleared by the loop's own
    /// `swap(0)` would be gone before that read ever looked.
    ///
    /// A station is closed rather than frozen (M7 §6.2): freezing suspends
    /// the stall timer, so a stalled live read that is frozen never wakes at
    /// all. Aimed at the generation observed *before* the send, exactly like
    /// `submit_seek` — a superseded generation means the worker is no longer
    /// blocked in anything this had to wake.
    pub fn submit_pause(&self) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(PlaybackCommand::Pause);
        if admission == Admission::Accepted {
            if self.traits.indefinite.load(Ordering::Acquire) {
                self.source_interrupt.retire_generation(generation);
            } else {
                self.source_interrupt.freeze();
            }
            let _ = self.wake.try_send(());
        }
        admission
    }

    /// `Play`, plus the level's release.
    pub fn submit_play(&self) -> Admission {
        let admission = self.try_send(PlaybackCommand::Play);
        if admission == Admission::Accepted {
            self.source_interrupt.thaw();
            let _ = self.wake.try_send(());
        }
        admission
    }

    /// Out-of-band, like `interrupt_stop`: nothing queues, because a stop is
    /// never refused.
    pub fn submit_stop(&self) {
        self.interrupt_stop();
    }

    /// Out-of-band, like `interrupt_shutdown`.
    pub fn submit_shutdown(&self) {
        self.interrupt_shutdown();
    }

    /// The interrupt every source this session opens shares, for a caller
    /// (a test harness among them) that needs to act on it directly.
    pub fn source_interrupt(&self) -> Arc<SourceInterrupt> {
        Arc::clone(&self.source_interrupt)
    }

    /// Install or remove the HTTP service `Worker::load` builds a
    /// `PrepareContext` from. `None` (the default) means every `Load` of an
    /// `Http` source fails with `RemoteFailure::InvalidSource` - "no HTTP
    /// service in this session" - exactly as `prepare::open_http` already
    /// documents.
    pub fn set_http(&self, service: Option<Arc<HttpService>>) {
        *lock(&self.http) = service;
    }

    /// Install the reconnect timing every later outage is judged against
    /// (M7 §7). The default is `ReconnectPolicy::default()`; a test installs
    /// a brisk one so a whole outage fits inside a test's patience.
    pub fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *lock(&self.reconnect_policy) = policy;
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Joins the worker, then drains what it left behind.
    ///
    /// The channel drain is safe only because the thread has already gone —
    /// nothing can send again — and that is the same happens-before the final
    /// `Progress` rests on. Channel first, backlog after: everything the worker
    /// flushed was emitted before anything it could not.
    pub fn join(mut self) -> ShutdownReport {
        let backlog = match self.worker.take() {
            Some(worker) => match worker.join() {
                Ok(events) => events,
                Err(panic) => {
                    tracing::warn!(
                        panic = %describe_panic(&panic),
                        "the decode worker panicked; its shutdown backlog is lost"
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // After the decode worker, whose teardown retires the last mapping.
        self.spectrum_thread.stop();
        let mut events = Vec::new();
        while let Ok(event) = self.stream.events.try_recv() {
            events.push(event);
        }
        events.extend(backlog);
        ShutdownReport {
            progress: self.progress(),
            events,
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.interrupt_shutdown();
        // Idempotent after `join`. The spectrum thread wakes on this at once
        // and never waits on the decode worker, so this cannot hang.
        self.spectrum_thread.stop();
    }
}

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left. Same pattern as
/// `http::channel::lock` and `wait::lock`.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The transport state a blocked source read may service, behind one lock.
///
/// `Handshake` and `Timeline` live here rather than on `Worker` because the
/// wait hook needs them and `Worker` is `&mut`-shaped: the hook runs from
/// inside `pump_audio -> source.next_planar()`, which already holds
/// `&mut self.source`, so it cannot also reach the rest of `Worker` and
/// `unsafe_code = "forbid"` rules out a lifetime-erased slot back onto it.
/// Contention is nil: the hook runs only while the worker is blocked inside a
/// decoder read, which is precisely when the worker is not touching any of
/// this (Ruling 1's lock order note lives in `wait.rs`, which is the module
/// that actually has two lock-takers to order).
///
/// `pcm`, `link` and `config` stay off this struct and live as plain `Worker`
/// fields instead: `pcm` is `rtrb::Producer<f32>`, which is `!Sync` and must
/// stay on the worker regardless — the hook must never push audio — and
/// `link`/`config` are not needed by the hook at all (`Handshake` already
/// holds its own clone of `link`), so putting them under this lock would only
/// widen the hook's reach for no benefit and force `pump_audio` to touch this
/// lock for information it does not need (Ruling 3).
pub struct TransportCore {
    handshake: Handshake,
    timeline: Timeline,
    /// Media position the current generation's frame counting starts from.
    anchor: Duration,
    sample_rate: u32,
    /// This transport's spectrum tap, when it has one: what every `Run`
    /// publication must map first (decision 18).
    tap: Option<TransportTap>,
}

/// What a transport needs to publish its tap mappings: the registry, its
/// never-reused instance ID, and the revision and channel count it plays.
pub(crate) struct TransportTap {
    registry: TapRegistry,
    instance: u64,
    session_rev: u64,
    channels: u16,
}

impl TransportCore {
    /// `pub`, unlike the rest of this impl block, so a test can build a real
    /// `TransportCore` and exercise `WaitService::service_as` against it with
    /// no `Worker` in the loop at all — the only way to prove the clock path
    /// `observed_position` depends on actually advances a published position,
    /// which nothing in this crate's `tests/` could otherwise reach.
    pub fn new(
        handshake: Handshake,
        timeline: Timeline,
        anchor: Duration,
        sample_rate: u32,
    ) -> Self {
        Self {
            handshake,
            timeline,
            anchor,
            sample_rate,
            tap: None,
        }
    }

    /// Attaches the spectrum tap's mapping state. Builder-style, so `new`
    /// keeps its public signature.
    pub(crate) fn with_tap(mut self, tap: TransportTap) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Publishes the mapping for the `Run` about to be published for
    /// `generation`, under the epoch that publication will use. Must run
    /// immediately before `start_running` or `release` (decision 18).
    fn publish_tap_mapping(&self, generation: u16) {
        if let Some(tap) = &self.tap {
            tap.registry.publish(TapMapping {
                instance: tap.instance,
                generation,
                epoch: self.handshake.upcoming_epoch(),
                session_rev: tap.session_rev,
                sample_rate: self.sample_rate,
                channels: tap.channels,
            });
        }
    }

    /// Retires every mapping of this transport's tap, for teardown.
    fn retire_tap(&self) {
        if let Some(tap) = &self.tap {
            tap.registry.retire_instance(tap.instance);
        }
    }

    /// Publishes `Run` for `generation`, mapping it first.
    fn start_running(&mut self, generation: u16) {
        self.publish_tap_mapping(generation);
        let Self {
            handshake,
            timeline,
            ..
        } = self;
        handshake.start_running(generation, timeline);
    }

    /// Drain spans and return the position implied by what has actually
    /// played.
    pub(crate) fn observed_position(&mut self, now: Nanos) -> Duration {
        self.handshake.drain_spans(&mut self.timeline);
        let played = self.timeline.played_frames(now);
        self.anchor + frames_to_duration(played, self.sample_rate)
    }

    pub(crate) fn quality(&self) -> PositionQuality {
        self.timeline.quality()
    }

    /// Park the callback, for the hook's freeze arm. Returns whether it was
    /// acknowledged, exactly as `Handshake::park`'s `Result` does, just
    /// flattened to a `bool` since the caller only branches on which.
    pub(crate) fn park(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> bool {
        self.handshake
            .park(&mut self.timeline, pump, deadline)
            .is_ok()
    }

    /// Release a parked callback: the hook's thaw arm and `Worker::play`'s
    /// resume. The mapping goes out first (decision 18).
    pub(crate) fn release(&mut self) {
        self.publish_tap_mapping(self.handshake.generation());
        self.handshake.release();
    }
}

/// M10 §4: what a resume must establish, held until a landing is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingResume {
    Seek(Duration),
    Restart,
}

impl PendingResume {
    fn target(self) -> Duration {
        match self {
            Self::Seek(target) => target,
            Self::Restart => Duration::ZERO,
        }
    }
}

struct Worker {
    output: Box<dyn AudioOutput>,
    faults: Receiver<OutputFault>,
    transport: Arc<Mutex<Option<TransportCore>>>,
    /// The PCM producer side of the ring. `!Sync`, so it stays here rather
    /// than behind `transport`'s lock — see `TransportCore`'s doc comment.
    pcm: Option<rtrb::Producer<f32>>,
    link: Option<Arc<OutputLink>>,
    config: Option<NegotiatedOutput>,
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
    capabilities: MediaCapabilities,
    converter: Option<Converter>,
    /// Interleaved output samples the ring has not accepted yet.
    staging: Vec<f32>,
    state: PlaybackState,
    session_rev: u64,
    /// The logical resume point. Stop and recreation preserve it; load,
    /// restart and successful seeks establish a new one.
    position: Duration,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3, M3.1). Sticky: ordinary playback never changes it, and a seek
    /// that performs no actual `seek_refined` call (`reseek`'s no-op, or a
    /// load that never moves off zero) leaves it exactly as it was, never
    /// resetting it to `Established` by default. `publish_progress` mirrors
    /// this into `facts.provenance` on every pass, the same way it mirrors
    /// `position` and `degraded`.
    position_provenance: PositionProvenance,
    /// A seek or restart stored but not yet established (M10 §4). Read, never
    /// taken, by every resume; cleared only once a landing is installed.
    pending: Option<PendingResume>,
    /// The established duration of a finite source in recovery, cached on
    /// entry because the decoder that knew it is gone (M10 §3). Clamps a seek
    /// stored offline. Cleared by `load`.
    recovery_duration: Option<Duration>,
    media: Option<MediaId>,
    /// The load currently in flight, set by `load` before `StateChanged`
    /// announces `Loading` and taken (cleared) the moment that load reaches
    /// an outcome: `Loaded` takes it into `adopted_load`, and both
    /// cancellation returns plus `fail_with` clear it directly (M5 §6).
    loading: Option<LoadRequestId>,
    /// The load whose media the worker currently holds, set when `Loaded` is
    /// emitted. Stop, pause, seek and device recovery never touch it; only
    /// the next `load` clears it, before announcing its own `Loading`.
    adopted_load: Option<LoadRequestId>,
    volume: Volume,
    generation: u16,
    pushed_total: u64,
    source_eof: bool,
    converter_flushed: bool,
    decoder_drained: bool,
    degraded: bool,
    /// Whether an attempt (a station's `fresh_open`, a finite `prime_attempt`)
    /// is priming a source it has not committed to yet (M7 §5.2, M10 §5).
    /// While set, a read failure is recorded in `attempt_failure` instead of
    /// transitioning the session.
    attempting: bool,
    attempt_failure: Option<PlaybackError>,
    shutting_down: bool,
    receivers_gone: bool,
    pending_events: VecDeque<PlaybackEvent>,
    commands: Receiver<PlaybackCommand>,
    events: Sender<PlaybackEvent>,
    liveness: Sender<()>,
    wake: Receiver<()>,
    interrupt: Arc<AtomicU8>,
    xruns: u64,
    lost_spans: u64,
    dropped_events: u64,
    /// A collapsed fault held back because the event backlog had no room for
    /// the events acting on it would produce.
    deferred_fault: Option<OutputFault>,
    device_warnings: u64,
    reported_diagnostics: u64,
    last_diagnostic: Instant,
    /// The scalars the shared `service` needs, mirrored from the fields above
    /// on every `publish_progress` pass. Lock order: this before `transport`
    /// (Ruling 1) — enforced by `WaitService`, which is the only thing that
    /// ever takes both.
    facts: Arc<Mutex<SessionFacts>>,
    /// Shared with the wait hook wired to every open remote source: the one
    /// implementation `publish_progress` and `WaitHook::service` both call
    /// (Ruling 4).
    service: Arc<WaitService>,
    /// The one `SourceInterrupt` this worker's whole life uses (Carried
    /// Finding 2). Handed to `prepare` as `PrepareContext::interrupt` for
    /// every source this worker ever opens, and to `WaitService` at
    /// construction — the same instance throughout, never swapped.
    source_interrupt: Arc<SourceInterrupt>,
    /// The handle's out-of-band view of `capabilities`, written only by
    /// `set_capabilities` (M7 §6.1).
    traits: Arc<SourceTraits>,
    /// Shared with `EngineHandle::set_http`, so a service installed after
    /// this worker was spawned is visible the next time `load` builds a
    /// `PrepareContext`.
    http: Arc<Mutex<Option<Arc<HttpService>>>>,
    /// Shared with `EngineHandle::set_reconnect_policy`. Read fresh at every
    /// decision point, never cached, so an installed policy takes effect at
    /// once (M7 §7).
    reconnect_policy: Arc<Mutex<ReconnectPolicy>>,
    /// The outage in progress: `Some` from the first retryable failure of an
    /// established live connection until sustained playback ends it, a
    /// failure ends the session (every one of them, through `fail_with`), or
    /// the listener ends the request (pause, stop, a replacing load,
    /// shutdown). `None` at every other moment - which is what lets each
    /// outage be judged against a budget of its own.
    outage: Option<Outage>,
    /// Where heard-time accounting last read the current generation (M10
    /// §6): its number and how far past its anchor the position was.
    heard_mark: Option<(u16, Duration)>,
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
    device_clock: Arc<AtomicU64>,
    /// Mirrors whether `pending_events` is empty, so `WaitService::announce`
    /// can tell whether jumping the worker's own backlog would misreport
    /// event order. Maintained after every mutation of `pending_events`.
    backlog_empty: Arc<AtomicBool>,
    /// Builds each transport's spectrum tap and holds the registry its
    /// mappings are published to.
    spectrum: SpectrumPort,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        progress: Arc<Mutex<Progress>>,
        commands: Receiver<PlaybackCommand>,
        events: Sender<PlaybackEvent>,
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
        let backlog_empty = Arc::new(AtomicBool::new(true));
        let outbox = Arc::new(Mutex::new(VecDeque::new()));
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
            events.clone(),
            outbox,
            Arc::clone(&backlog_empty),
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
            pending_events: VecDeque::new(),
            commands,
            events,
            liveness,
            wake,
            interrupt,
            xruns: 0,
            lost_spans: 0,
            dropped_events: 0,
            deferred_fault: None,
            device_warnings: 0,
            reported_diagnostics: 0,
            last_diagnostic: Instant::now()
                .checked_sub(DIAGNOSTIC_INTERVAL)
                .unwrap_or_else(Instant::now),
            facts,
            service,
            source_interrupt,
            traits,
            http,
            reconnect_policy,
            outage: None,
            heard_mark: None,
            device_clock,
            backlog_empty,
            spectrum,
        }
    }

    fn run(mut self) -> Vec<PlaybackEvent> {
        let commands = self.commands.clone();
        let wake = self.wake.clone();
        let liveness = self.liveness.clone();
        loop {
            // 0. Drain whatever the hook announced while a read was blocked,
            //    onto the BACK of pending_events, before anything else can
            //    emit (Ruling 5). Two reasons this has to be first, ahead of
            //    even step 1's interrupt handling:
            //    - Step 1's stop branch calls `do_stop`, which emits
            //      `StateChanged{Stopped}`. Draining after it would put a
            //      `Paused` the hook announced *earlier* behind it.
            //    - Step 1's shutdown branch returns `pending_events`
            //      immediately. Anything still in the outbox at that point
            //      would never reach the shutdown report, breaking D19's
            //      losslessness for exactly the events a stalled read
            //      produced. `shutdown()` also drains it, as its own first
            //      action, so every exit out of this loop carries it.
            self.drain_outbox();

            // 1. Out-of-band interrupts. Shutdown dominates stop.
            let flags = self.interrupt.swap(0, Ordering::Acquire);
            if flags & SHUTDOWN != 0 {
                self.shutdown();
                return Vec::from(self.pending_events);
            }
            if flags & STOP != 0 {
                self.do_stop();
            }
            // SEEK carries no worker-side action. Its retirement woke the
            // read, and the queued `SeekTo` carries the target; the
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

            // 5. Diagnostics coalesce; they never enter pending_events.
            self.emit_aggregated_warning_if_slot_free();

            // 6. Wait. Never a bare sleep - stop and shutdown must wake it.
            let admit = self.pending_events.is_empty();
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
                    return Vec::from(self.pending_events);
                }
                None => {}
            }
            if self.shutting_down {
                self.shutdown();
                return Vec::from(self.pending_events);
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
            // channel's own errors - `SendError` when flushing events, and
            // `RecvError` above when the command sender is gone - never from
            // channel occupancy, which says nothing about connectedness.
            if self.receivers_gone {
                self.shutdown();
                return Vec::from(self.pending_events);
            }
        }
    }

    // ----------------------------------------------------------------- events

    fn emit(&mut self, event: PlaybackEvent) {
        // Protected events — load outcomes — bypass `PENDING_CAP` entirely
        // (Decision 4): correlation cannot be inferred from anything later,
        // so one is always pushed, never dropped or counted as such.
        if self.pending_events.len() >= PENDING_CAP && !event.is_protected() {
            if !event.is_terminal() {
                self.dropped_events += 1;
                return;
            }
            // A terminal outcome displaces the oldest ordinary event rather
            // than being dropped: nothing that follows implies it.
            match displacement_victim(&self.pending_events) {
                Some(index) => {
                    self.pending_events.remove(index);
                    self.dropped_events += 1;
                }
                None => {
                    self.dropped_events += 1;
                    return;
                }
            }
        }
        self.pending_events.push_back(event);
        self.note_backlog();
    }

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

    /// Drain `WaitService`'s outbox onto the back of `pending_events`. Called
    /// at the very top of every loop pass, before step 1's interrupt
    /// handling, again in `check_end_of_track` before it can emit
    /// `EndOfTrack`/`StateChanged{Ended}` in the same pass a blocked read
    /// returned in, and again as `shutdown`'s own first action (Ruling 5) -
    /// see `run`'s step 0 comment for why the first and last are needed and
    /// why it must be the back, never the front.
    ///
    /// Routed through `emit` rather than pushing directly: the hook is a
    /// second emitter, and its events must answer to the same `PENDING_CAP`
    /// and terminal-displaces-oldest drop policy as the worker's own, or a
    /// source that freeze/thaws repeatedly during one long block could grow
    /// `pending_events` without limit and without `dropped_events` ever
    /// reflecting it.
    fn drain_outbox(&mut self) {
        for event in self.service.take_outbox() {
            self.emit(event);
        }
    }

    /// Mirrors whether `pending_events` is empty into `backlog_empty`, so
    /// `WaitService::announce` can tell whether jumping the worker's own
    /// backlog would misreport event order. Called after every mutation of
    /// `pending_events`.
    fn note_backlog(&self) {
        self.backlog_empty
            .store(self.pending_events.is_empty(), Ordering::Release);
    }

    fn free_event_slots(&self) -> usize {
        EVENT_CAPACITY.saturating_sub(self.events.len())
    }

    fn flush_events(&mut self) {
        while let Some(front) = self.pending_events.front() {
            let free = self.free_event_slots();
            let may_use_reserve = front.is_terminal() || front.is_protected();
            if free == 0 || (!may_use_reserve && free <= RESERVED_EVENT_SLOTS) {
                break;
            }
            let Some(event) = self.pending_events.pop_front() else {
                break;
            };
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.pending_events.push_front(event);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.receivers_gone = true;
                    self.note_backlog();
                    return;
                }
            }
        }
        self.note_backlog();
    }

    /// Counters, not a queue: an aggregated warning goes out only when an
    /// ordinary slot is free, and it reports the running totals. A receiver
    /// that stays connected without draining therefore bounds diagnostics by
    /// the channel itself, not by a rate limit alone.
    fn emit_aggregated_warning_if_slot_free(&mut self) {
        let total = self.xruns + self.lost_spans + self.dropped_events + self.device_warnings;
        if total == self.reported_diagnostics
            || !self.pending_events.is_empty()
            || self.free_event_slots() <= RESERVED_EVENT_SLOTS
            || self.last_diagnostic.elapsed() < DIAGNOSTIC_INTERVAL
        {
            return;
        }
        let message = format!(
            "audio glitches: {} underruns, {} lost spans, {} dropped events, {} device warnings",
            self.xruns, self.lost_spans, self.dropped_events, self.device_warnings
        );
        self.reported_diagnostics = total;
        self.last_diagnostic = Instant::now();
        let event = PlaybackEvent::Warning {
            session_rev: self.session_rev,
            message,
        };
        if let Err(TrySendError::Disconnected(_)) = self.events.try_send(event) {
            self.receivers_gone = true;
        }
    }

    fn warn(&mut self, message: String) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::Warning {
            session_rev,
            message,
        });
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
    fn announce_playing(&mut self) {
        if lock(&self.transport).is_some() {
            self.set_state(PlaybackState::Playing);
        }
    }

    fn set_state(&mut self, state: PlaybackState) {
        if self.state == state {
            return;
        }
        self.state = state;
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

    fn fail(&mut self, message: String) {
        self.fail_with(message, None);
    }

    /// `fail`'s general form. A remote failure carries a typed `cause`
    /// alongside `message`, so a policy can act on what went wrong rather
    /// than only read a string a human wrote; M1's local failures have none,
    /// which is why `fail` is still the entry point every local call site
    /// uses (Ruling 1).
    fn fail_with(&mut self, message: String, cause: Option<RemoteFailure>) {
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
    fn fail_from(&mut self, error: PlaybackError) {
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

    /// Mirror the worker's own truth into `SessionFacts`, then run the same
    /// `service` the hook does (Ruling 4) — which is what actually recomputes
    /// the live position from the transport while playing, publishes the
    /// `Progress` snapshot, and services the freeze level. One implementation,
    /// two callers.
    ///
    /// Two writers keep `device_clock` current, for two different reasons.
    /// `CallbackCore::fill` writes it on every callback invocation, including
    /// while parked or frozen, which is what keeps the recompute below seeing
    /// the clock advance while the worker itself is stuck inside a blocked
    /// decoder read and this method is not running at all — Cause A this
    /// exists to fix. This method *also* writes it, immediately before
    /// `service_as`, because it is the only writer that can be exactly as
    /// fresh as `self.output.now()` at the instant this recompute actually
    /// runs: `CallbackCore::fill` only fires once per output buffer period
    /// (2 ms with this crate's own `TestOutput` harness, and it is what
    /// `capture_position`'s protocol round-trip is compared against in
    /// `tests/engine_contract.rs`), so relying on it alone during an ordinary
    /// loop pass would leave the published position up to one buffer period
    /// behind a freeze-captured one taken moments later — which is exactly
    /// the discrepancy that broke `stop_preserves_the_logical_position` and
    /// its siblings the first time this was tried. Reading `self.output.now()`
    /// unconditionally is safe even while idle: it can return `Nanos(0)`
    /// (`CpalOutput::now()`'s `None` arm) when no stream is open, but nothing
    /// ever reads `device_clock` unless `facts.playing` is true, and that is
    /// only ever true when a transport - and so a stream - exists.
    ///
    /// `fetch_max`, not `store`, for the same reason `CallbackCore::fill`
    /// uses it: this write and the callback's race on two different threads
    /// with no ordering between them, so a plain `store` could let a
    /// just-sampled-but-not-yet-stored older instant from one thread
    /// overwrite a newer value the other already published, and the reader
    /// would see position step backward. `fetch_max` only ever moves this
    /// forward, which is correct for both writers *within one clock
    /// domain* — the domain reset that must still win with a plain `store`
    /// lives in `open_transport`, not here.
    fn publish_progress(&mut self) {
        self.device_clock
            .fetch_max(self.output.now().0, Ordering::Relaxed);
        {
            let mut facts = lock(&self.facts);
            facts.session_rev = self.session_rev;
            facts.media = self.media.clone();
            facts.position = self.position;
            facts.degraded = self.degraded;
            // Mirrored, never derived from `degraded` or `playing` (§3.1's
            // stickiness): playing on from an estimated landing must keep
            // reporting it estimated for as long as the landing stands.
            facts.provenance = self.position_provenance;
            // Paused counts as well as Playing: parking silences the
            // callback, but the frames it already handed to the device still
            // play out, so the position goes on rising for one output
            // latency after the park and only then settles. Freezing the
            // number at the instant of the park would report a position
            // slightly behind what the listener actually heard.
            // `Reconnecting` too (M7 §7): the transport is left running so
            // the ring plays out after a disconnect, and that audio is heard,
            // so it must be counted.
            facts.playing = matches!(
                self.state,
                PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Reconnecting
            );
            // `frozen_by_hook` is deliberately left untouched here: it is the
            // hook's own bookkeeping (Ruling 1's `SessionFacts` lives in
            // `wait.rs`), and this pass has nothing new to tell it.
            facts.load = self.adopted_load;
        }
        self.service.service_as(Servicing::WorkerLoop);
        // Read back whatever the shared recompute settled on, so
        // `self.position` - the field every command (`SeekBy` among them)
        // reads as "the current position" - stays live while playing, exactly
        // as this method used to keep it before the recompute moved into
        // `WaitService` so the hook could share it.
        self.position = lock(&self.facts).position;
    }

    /// Read the link's counters before anything else can swap them away, and
    /// forward the span losses to the timeline that has to account for them.
    ///
    /// Runs before `publish_progress`: `Handshake::drain_spans` (invoked
    /// inside `TransportCore::observed_position`, which `publish_progress`
    /// reaches through `service_as`) reads the same link and forwards
    /// `spans_dropped` into the timeline itself, so reading it here first is
    /// what makes that second read come back zero and leaves the timeline as
    /// the one place span losses actually get counted.
    fn collect_diagnostics(&mut self) {
        let Some(link) = self.link.as_ref() else {
            return;
        };
        let diagnostics = link.take_diagnostics();
        {
            let mut guard = lock(&self.transport);
            if let Some(core) = guard.as_mut() {
                core.timeline.note_dropped(diagnostics.spans_dropped);
            }
        }
        self.lost_spans += u64::from(diagnostics.spans_dropped);
        // Underruns after the decoder has run dry are the expected sound of a
        // track ending, not a glitch worth reporting.
        if !self.decoder_drained {
            self.xruns += u64::from(diagnostics.xruns);
        }
    }

    // ----------------------------------------------------------------- faults

    /// Room a fault must leave behind before it is acted on.
    ///
    /// Two for the fault's own pair (`Failed` + `StateChanged`, or
    /// `DeviceRecovered` + `StateChanged`), plus the terminal work that can
    /// still follow it: end of track is another two, and a load in the same
    /// pass is four (`StateChanged{Loading}`, `Loaded`, `CapabilitiesChanged`,
    /// `StateChanged{Paused}`) now that a load whose initial seek proves a
    /// remote source's previously-unknown demuxer support emits
    /// `CapabilitiesChanged` on top of `Loaded` (`RESERVED_EVENT_SLOTS`'s own
    /// comment already carries this same four). Reserving only its own pair
    /// let a fault land at 126 pending, leaving no room for the EOF that
    /// followed - which then evicted a lifecycle event.
    const FAULT_EVENT_BUDGET: usize = 8;

    fn service_faults(&mut self) {
        let mut worst: Option<OutputFault> = self.deferred_fault.take();
        let mut seen = 0u64;
        // Collapse the queue to its most severe entry: acting on each one in
        // turn would let a burst of faults generate a burst of events.
        while let Ok(fault) = self.faults.try_recv() {
            seen += 1;
            if worst.is_none_or(|current| severity(fault) > severity(current)) {
                worst = Some(fault);
            }
        }
        let Some(fault) = worst else {
            return;
        };
        // Command admission already stops the loop creating events faster than
        // they drain. Faults arrive asynchronously and answer to no such gate,
        // so apply the same rule here: hold the fault until its events fit,
        // rather than acting now and dropping the report at the cap.
        if self.pending_events.len() + Self::FAULT_EVENT_BUDGET > PENDING_CAP {
            self.deferred_fault = Some(fault);
            self.device_warnings += seen;
            return;
        }
        self.device_warnings += seen;
        match fault {
            // Rerouted or refused realtime: playback continues, but the timing
            // base underneath the timeline may have jumped.
            OutputFault::Recoverable(_) => self.degraded = true,
            OutputFault::Rebuild(kind) => {
                let resume = self.state == PlaybackState::Playing;
                // Nothing to report here: `rebuild` has already emitted
                // whatever the outcome warrants.
                let _ = self.rebuild(&format!("{kind:?}"), resume);
            }
            OutputFault::Fatal(kind) => self.fail(format!("audio device failed: {kind:?}")),
        }
    }

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
    fn rebuild(&mut self, reason: &str, resume: bool) -> Result<(), PlaybackError> {
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
            let target = self.position;
            match self.reseek(target) {
                Ok((actual, provenance)) => {
                    self.position = adopt_preserved(target, actual);
                    self.position_provenance = provenance;
                }
                Err(error) => {
                    if !is_cancelled(&error) {
                        self.fail(format!("cannot recover after {reason}: {error}"));
                    }
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
        let generation = self.generation;
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
    fn fresh_open(&mut self) -> Result<(), PlaybackError> {
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
    fn abandon_attempt(&mut self) {
        self.teardown();
        self.source_interrupt.retire();
        self.retire_remote_source();
    }

    /// M10 §5: one recovery attempt for a finite source.
    ///
    /// `Ok` means `Playing` was announced and the pending intent, if any, was
    /// reported. On `Err` nothing of the attempt remains: no transport, no
    /// decoder, `pending` untouched and the position as it was captured.
    fn reconnect_finite(&mut self) -> Result<(), PlaybackError> {
        // 1. Capture and tear down before any network I/O, so the ring cannot
        //    move once the position is read. A no-op after the first attempt,
        //    which already left no transport.
        self.capture_and_teardown();
        let previous = (self.position, self.position_provenance);
        // 2.
        let target = self.pending.map_or(self.position, PendingResume::target);
        // 3 + 4. `ensure_source_open` sets `expected = Finite`, so a reopen
        //    that answers as a station fails with `ResourceChanged`.
        let landing = self.ensure_source_open().and_then(|_| self.reseek(target));
        let (actual, provenance) = match landing {
            Ok(landing) => landing,
            Err(error) => {
                self.abandon_attempt();
                return Err(error);
            }
        };
        // 5. The landing becomes the anchor: `open_transport` copies
        //    `self.position` into the new `TransportCore`, so a landing
        //    assigned afterwards would play from the target while progress
        //    and every later capture counted from the old position.
        self.position = adopt_preserved(target, actual);
        self.position_provenance = self.landing_provenance(actual, provenance);
        // 6 + 7.
        if let Err(error) = self.prime_attempt() {
            (self.position, self.position_provenance) = previous;
            return Err(error);
        }
        // 8.
        self.start_running();
        self.announce_playing();
        self.complete_pending(actual);
        Ok(())
    }

    /// M10 §5 steps 6–7, shared by `reconnect_finite` and `restore`: install
    /// and prime with a read failure reported here rather than through
    /// `fail_with`, then commit only if nothing failed and nothing cancelled.
    /// `Ok` leaves the transport primed and parked, for the caller to start;
    /// `Err` leaves no transport and no decoder.
    ///
    /// No `primed` check, unlike `fresh_open`: a finite target at the very
    /// end primes nothing and is still a landing, which end-of-track follows.
    fn prime_attempt(&mut self) -> Result<(), PlaybackError> {
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
        let generation = self.generation;
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
    fn start_running(&mut self) {
        let mut guard = lock(&self.transport);
        if let Some(core) = guard.as_mut() {
            let generation = core.handshake.generation();
            core.start_running(generation);
        }
    }

    /// Whether a stop or a shutdown is pending. Read rather than consumed:
    /// the loop's own `swap(0)` is what acts on it.
    fn interrupted(&self) -> bool {
        stop_or_shutdown(self.interrupt.load(Ordering::Acquire))
    }

    /// M10 §6: credit the outage with what was heard since the last reading.
    /// A new generation (a seek, a reopen, a device rebuild) starts again from
    /// its own anchor, so a seek moves nothing here.
    fn settle_heard(&mut self) {
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

    /// Freeze the callback and read back the frames it really played.
    ///
    /// Returns `false` when the device did not answer, in which case the
    /// caller must fall back to the link's rescue record.
    fn capture_position(&mut self) -> bool {
        let Some(rate) = self.config.as_ref().map(|c| c.sample_rate) else {
            return true;
        };
        let captured = {
            let Self {
                transport, output, ..
            } = self;
            let mut guard = lock(transport);
            let Some(core) = guard.as_mut() else {
                return true;
            };
            let anchor = core.anchor;
            let TransportCore {
                handshake,
                timeline,
                ..
            } = core;
            let mut clock = || output.now();
            let mut pump = || std::thread::sleep(PUMP_NAP);
            handshake
                .freeze_and_capture(timeline, &mut clock, &mut pump, DEADLINE)
                .map(|frames| anchor + frames_to_duration(frames, rate))
        };
        match captured {
            Ok(position) => {
                self.position = position;
                self.settle_heard();
                true
            }
            Err(_) => false,
        }
    }

    fn capture_and_teardown(&mut self) {
        let rate = self
            .config
            .as_ref()
            .map(|c| c.sample_rate)
            .unwrap_or(48_000);
        let exact = self.capture_position();
        // Read before `teardown` clears the `TransportCore` this comes from.
        // Unused when `exact` is true (the common case) or when there was
        // never a transport to begin with - the rescue branch below is the
        // only place it matters, and it is reachable only when both a
        // transport existed and its capture timed out.
        let anchor = lock(&self.transport)
            .as_ref()
            .map(|core| core.anchor)
            .unwrap_or(self.position);
        let generation = self.generation;
        let link = self.teardown();
        if exact {
            return;
        }
        // The capture timed out. The callback's last unpublished span survives
        // in the link, so read it once the teardown has joined the backend
        // thread; if it is unusable, keep the last validated position rather
        // than fabricating one.
        self.degraded = true;
        if let Some(link) = link
            && let Some(record) = link.take_rescue_after_teardown()
            && record.generation == generation
        {
            self.position = anchor + frames_to_duration(record.media_total_after, rate);
        }
    }

    fn teardown(&mut self) -> Option<Arc<OutputLink>> {
        // Close first: dropping the stream joins the backend thread, which is
        // what makes the link's rescue slot safe to read afterwards.
        self.output.close();
        retire_faults(&mut self.deferred_fault, &self.faults);
        let link = self.link.take();
        // A load, stop, recovery or shutdown invalidates every mapping of the
        // old transport, so its spectrum disappears without new audio.
        if let Some(core) = lock(&self.transport).take() {
            core.retire_tap();
        }
        self.pcm = None;
        self.config = None;
        self.converter = None;
        self.staging.clear();
        self.pushed_total = 0;
        self.source_eof = false;
        self.converter_flushed = false;
        self.decoder_drained = false;
        link
    }

    fn shutdown(&mut self) {
        // Ruling 5: drained here too, as this method's own first action, so
        // every exit out of `run()` carries whatever the hook announced while
        // a read was blocked - see `run`'s step 0 comment. `capture_position`
        // and `teardown` below can only emit through `pending_events`
        // (neither calls `self.emit` directly), so nothing between here and
        // `publish_progress` can reorder ahead of what was just drained.
        self.drain_outbox();
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
        // MINOR 5 (fix round 1): `descriptor`'s own doc comment says "set by
        // `load`, cleared by `shutdown`" - make that true rather than leave
        // it stale. Latent either way, since every `shutdown()` call site
        // returns immediately after, but a stale value contradicts the
        // comment describing it.
        self.descriptor = None;
        self.state = PlaybackState::Idle;
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
                    // IMPORTANT 2 (fix round 1): a retired remote source is
                    // `None` here with `descriptor` still naming it - the
                    // very next pass after `is_retired_read`'s own arm ran,
                    // since `SEEK` takes no step-1 action and `select!` can
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
    fn source_ended(&mut self, failure: RemoteFailure) {
        if self.attempting {
            self.attempt_failure = Some(failure.into());
            self.source = None;
            return;
        }
        if retryable(&failure, Continuity::Indefinite) {
            self.enter_reconnecting(failure);
        } else {
            self.fail_with(format!("{failure}"), Some(failure));
        }
    }

    /// M10 §3: the failure a finite read error recovers from, or `None` when
    /// it takes today's path. Only a playing, remote session whose resume
    /// capability is `Supported` recovers, and only from a failure the retry
    /// table calls retryable for finite media.
    fn recoverable(&self, error: &PlaybackError) -> Option<RemoteFailure> {
        let eligible = self.state == PlaybackState::Playing
            && self.source_is_remote()
            && self.capabilities.resume_capability() == ResumeCapability::Supported;
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
        self.enter_reconnecting(failure);
    }

    /// M7 §7, M10 §3. A playing connection, or an attempt, failed retryably.
    ///
    /// The outage is the thing that spans attempts: it is begun by the first
    /// such failure and outlives every attempt until sustained playback ends
    /// it, the budget gives up, or the listener ends the request.
    fn enter_reconnecting(&mut self, failure: RemoteFailure) {
        let now = Instant::now();
        let policy = *lock(&self.reconnect_policy);
        let outage = self.outage.get_or_insert_with(|| Outage::begin(now));
        match outage.failed(now, &policy) {
            Next::GiveUp => self.fail_with(format!("{failure}"), Some(failure)),
            Next::AttemptAt(_) => {
                if self.is_indefinite() {
                    tracing::info!(reason = %failure, "live source lost; reconnecting");
                } else {
                    let position = self.position;
                    tracing::info!(reason = %failure, ?position, "connection lost; reconnecting");
                }
                // The output transport stays up so the ring plays out; only
                // the source goes.
                self.source_interrupt.retire();
                self.retire_remote_source();
                self.set_state(PlaybackState::Reconnecting);
            }
        }
    }

    /// Run from the loop, never from a sleep: commands stay serviced through
    /// every backoff, so a pause, a stop, a replacing load or a shutdown ends
    /// the attempts at once rather than after the current one.
    fn service_reconnect(&mut self) {
        match self.state {
            PlaybackState::Playing => {
                self.settle_heard();
                let policy = *lock(&self.reconnect_policy);
                if self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.is_over(&policy))
                {
                    self.outage = None;
                }
            }
            PlaybackState::Reconnecting => {
                // M7 §6: a cancellation already in hand ends recovery *now*.
                // An attempt begun here blocks this whole pass inside an open
                // that may last `limits.headers`, or inside a priming read a
                // silent station never answers; the stop, pause or replacing
                // load that cancels it is only looked at when the loop comes
                // back round, so recovery would outlive the explicit Play it
                // belongs to by a whole attempt. The interrupt word covers a
                // stop or a shutdown raised since step 1 consumed the flags
                // (the loop's `select!` is free to answer the wake that
                // carried one and leave the flag standing for the next pass);
                // a queued command covers a pause or a replacing load, which
                // travel on the command channel and cannot be peeked at. Any
                // queued command is reason enough to come back for it: the
                // loop takes one per pass, so an attempt is postponed by a
                // pass, never dropped. Commands are only taken while nothing
                // is backed up, so a blocked outbox must not be allowed to
                // stall recovery indefinitely - hence the backlog test.
                let command_waiting = self.pending_events.is_empty() && !self.commands.is_empty();
                if self.interrupted() || command_waiting {
                    return;
                }
                if !self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.due(Instant::now()))
                {
                    return;
                }
                let attempt = if self.is_indefinite() {
                    self.fresh_open()
                } else {
                    self.reconnect_finite()
                };
                match attempt {
                    Ok(()) => {
                        if let Some(outage) = self.outage.as_mut() {
                            outage.playing_from();
                        }
                    }
                    // The command that cancelled it decides what happens next.
                    Err(error) if is_cancelled(&error) => {}
                    Err(error) => {
                        let failure = match error {
                            PlaybackError::Remote(failure) => failure,
                            other => match remote_cause(&other) {
                                Some(failure) => failure,
                                // Not the station's fault: a device that will
                                // not open is never retried against a remote
                                // budget.
                                None => {
                                    self.fail_from(other);
                                    return;
                                }
                            },
                        };
                        // The attempt may have torn the old transport down
                        // and announced nothing; stay in Reconnecting.
                        self.state = PlaybackState::Reconnecting;
                        if retryable(&failure, self.capabilities.continuity) {
                            self.enter_reconnecting(failure);
                        } else {
                            self.fail_with(format!("{failure}"), Some(failure));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// End of track fires only once every pushed frame has been played, which
    /// the timeline reports only after the final span's predicted play time
    /// has passed - not when the ring merely empties.
    fn check_end_of_track(&mut self) {
        // Symmetric with `shutdown()`: this is the one place besides `run()`'s
        // own step 0 that can emit in the same pass a blocked read returned
        // in — `pump_audio`, called just before this, is where that read
        // lives. Draining first, before the early returns below, closes the
        // window step 0 does not cover: a `EndOfTrack`/`StateChanged{Ended}`
        // this call is about to emit must not land ahead of a `Paused` the
        // hook announced earlier in the very same pass.
        self.drain_outbox();
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
            // IMPORTANT 1 (final review), site 6: `Cancelled` here means a
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
        // MINOR 7 (fix round 1): `Duration::default()` is zero, which would
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
            // IMPORTANT 1 (final review): a stop or shutdown landing during
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
        let mut decoded = prepared.source;
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
            // §4.3 (Task 6 fix round 1): the estimate always wins as the
            // seek target, unconditionally — it is a preference between two
            // already-known locations decided upstream in
            // `resume::restart_preference`, not a duration-validated choice,
            // so nothing here re-runs `decide_resume` against the decoded
            // duration the way `Candidate` does. `established` is reported
            // exactly as `restart_preference` computed it — R8's "never
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

        if start_at > Duration::ZERO {
            // Whether this attempt is the first demonstration of a demuxer
            // seek this source had not yet proven - read before the attempt,
            // since a successful seek below is exactly what promotes it.
            let was_unknown = self.capabilities.seek == SeekSupport::Unknown;
            // R2: this is one of `seek_refined`'s four call sites, routed
            // through the shared `seek_bounded` so the deadline and the
            // unconditional `Estimated` provenance apply here exactly as
            // they do to every other caller - a launch resume at a stored
            // position is exactly the case the field report's "two presses
            // of the right arrow" generalises from.
            let deadline = self.seek_deadline();
            let seek = seek_bounded(
                &mut decoded,
                &self.source_interrupt,
                &self.interrupt,
                deadline,
                start_at,
                None,
            );
            match seek {
                Ok((outcome, provenance)) => {
                    self.position = adopt_preserved(start_at, outcome.actual);
                    self.position_provenance = provenance;
                    // A successful seek on a source whose demuxer seek
                    // support was still unproven IS the proof (§6). Recording
                    // it here means `Loaded`'s own `capabilities` already
                    // reflects it, and a listener watching
                    // `CapabilitiesChanged` specifically sees the upgrade
                    // too, rather than only inferring it from a later seek.
                    if was_unknown {
                        decoded.note_demuxer_proven();
                        self.set_capabilities(decoded.capabilities());
                        self.emit_capabilities(self.capabilities);
                    }
                }
                // Abandon the load, leaving no decoder open. The interrupt
                // still stands and the loop's next pass acts on it.
                Err(error) if is_cancelled(&error) => {
                    self.cancel_load();
                    return;
                }
                Err(error) => {
                    self.fail_from(error);
                    return;
                }
            }
        }
        let session_rev = self.session_rev;
        let position = self.position;
        let capabilities = self.capabilities;
        // Takes `loading`: this outcome closes it. `adopted_load` now holds
        // the same token until the next `load` clears it (M5 §6, Decision 2).
        self.loading = None;
        self.adopted_load = Some(request);
        self.emit(PlaybackEvent::Loaded {
            session_rev,
            request,
            media,
            metadata: decoded.metadata().clone(),
            capabilities,
            position,
            disposition,
        });
        self.source = Some(decoded);
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
            // The state moves to `Loading` *before* `restore()` runs (fix
            // round 1, IMPORTANT 4): `fail_with`'s very first statement
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
        // milestone's invariant forbids. Checked *before* any reopen (fix
        // round 1, IMPORTANT 3): `self.capabilities` already answers this
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
            // IMPORTANT 1 (final review): the state a caller left this in
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
        let landed = match self.reseek(target) {
            Ok((actual, provenance)) => {
                self.position = adopt_preserved(target, actual);
                self.position_provenance = self.landing_provenance(actual, provenance);
                actual
            }
            // A stop or a shutdown arrived mid-refinement. The preserved
            // position still stands; the interrupt is handled by the loop.
            Err(error) if is_cancelled(&error) => return,
            Err(error) => {
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
                self.complete_pending(landed);
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
    fn complete_pending(&mut self, actual: Duration) {
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
                    refinement_truncated: false,
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

    /// A restart that lands at zero is exact, whatever provenance a reseek
    /// that had nothing to do carried forward (M10 §4).
    fn landing_provenance(
        &self,
        actual: Duration,
        provenance: PositionProvenance,
    ) -> PositionProvenance {
        if self.pending == Some(PendingResume::Restart) && actual == Duration::ZERO {
            PositionProvenance::Established
        } else {
            provenance
        }
    }

    /// Park the callback and leave everything else exactly as it is. The ring
    /// keeps its audio, the callback keeps counting from where it stopped, and
    /// the timeline keeps its floor, so resuming is a single `release`.
    fn pause(&mut self) {
        // First, ahead of the `Playing`-only guard below: a `Reconnecting`
        // station is pausable too, and a station never parks — it closes.
        if self.is_indefinite() {
            self.pause_indefinite();
            return;
        }
        if self.state != PlaybackState::Playing {
            return;
        }
        // Idempotent with respect to the hook: if it already parked the
        // transport for a freeze and announced `Paused` itself, that
        // announcement already stands, so this only updates local state
        // rather than parking (redundantly) and re-emitting.
        if lock(&self.facts).frozen_by_hook {
            self.state = PlaybackState::Paused;
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
        // `submit_pause` sets the freeze level on the same `source_interrupt`
        // this parked for, out of band, and the two can land in either order:
        // if `freeze()` reaches `service_as` (run every pass via
        // `publish_progress`, not only from inside a blocked read) before
        // this dispatch of the queued `Pause` does, the early return above
        // already handles it. If this dispatch runs first instead, nothing
        // has told the hook yet - and without reconciling here, `service_as`
        // would see `frozen && !frozen_by_hook` on the very next pass and
        // park an already-parked transport a second time, which the
        // handshake has no acknowledgment for. `play`'s resume branch
        // already clears this same flag; this is its missing counterpart.
        self.reconcile_frozen_by_hook();
        self.set_state(PlaybackState::Paused);
    }

    /// M7 §6.2. Both pause routes end here: the dispatched `Pause`, and the
    /// one where the hook parked first and already announced `Paused`.
    ///
    /// A station holds no connection while paused. Listening time survives in
    /// `self.position`, and the next Play rejoins the live edge from there.
    fn pause_indefinite(&mut self) {
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
            self.state = PlaybackState::Paused;
        } else {
            self.set_state(PlaybackState::Paused);
        }
    }

    /// Set `frozen_by_hook` to match reality after this dispatch parked the
    /// transport on its own, so `service_as` does not act on a stale
    /// mismatch on its very next pass.
    ///
    /// Conditional on the freeze level actually being active
    /// (`source_interrupt.is_frozen()`), not unconditional: a local pause
    /// dispatched the ordinary way, with no `submit_pause` and so no freeze
    /// level ever raised, must leave this exactly as it was. Setting it
    /// unconditionally trips `service_as`'s *other* branch instead -
    /// `!frozen && frozen_by_hook` reads as "release" - undoing the pause on
    /// the very next loop pass, which is what broke
    /// `pause_holds_the_position_still_and_resume_continues_from_it` the
    /// first time this was tried.
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
        // playing or stopped alike (fix round 1, IMPORTANT 3): checked
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
            // IMPORTANT 1 (final review), site 4: a stop or shutdown landing
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
                        // means "this source cannot seek" (IMPORTANT 1,
                        // final review, site 5).
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
        // Deliberately no pre-emptive `begin()` here (fix round 1, MINOR 8).
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
        // R2: this is one of `seek_refined`'s four call sites, routed
        // through the shared `seek_bounded` so the deadline applies here
        // exactly as it does to the other three - this is the user's own
        // seek, the case the field report is about.
        let deadline = self.seek_deadline();
        let outcome = {
            let Some(source) = self.source.as_mut() else {
                return;
            };
            // Not the SEEK bit: `submit_seek` sets it for exactly this
            // dispatch, and checking it here would make this seek cancel
            // its own first attempt.
            seek_bounded(
                source,
                &self.source_interrupt,
                &self.interrupt,
                deadline,
                target,
                Some(SEEK_BUDGET),
            )
        };
        match outcome {
            Ok((outcome, provenance)) => {
                // `refinement_truncated` is false when refinement ran into the
                // end of the media, so a short landing is checked separately.
                let truncated = outcome.refinement_truncated
                    || outcome.actual.saturating_add(RESUME_TOLERANCE) < target;
                let actual = outcome.actual;
                self.position = actual;
                self.position_provenance = provenance;
                self.pending = None;
                // §6: any demonstrated seek is proof, not only the trial
                // `verify_seek_support` runs for a stopped one - an ordinary
                // playing seek that lands is just as conclusive.
                if self.capabilities.seek == SeekSupport::Unknown
                    && let Some(source) = self.source.as_mut()
                {
                    source.note_demuxer_proven();
                    let capabilities = source.capabilities();
                    self.set_capabilities(capabilities);
                    self.emit_capabilities(capabilities);
                }
                if let Err(error) = self.reinstall(playing) {
                    if !is_cancelled(&error) {
                        self.fail(format!("cannot restart the audio device: {error}"));
                    }
                    return;
                }
                if self.state == PlaybackState::Ended {
                    self.set_state(PlaybackState::Paused);
                }
                // §11: requested/actual seek.
                tracing::debug!(requested = ?target, ?actual, truncated, "seek completed");
                let session_rev = self.session_rev;
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
            Err(error) if is_cancelled(&error) => {
                self.position = preserved;
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekCancelled {
                    session_rev,
                    requested: target,
                });
                // Best-effort restoration at the preserved position (§5.4): one
                // fresh bounded attempt, never the expired deadline this seek
                // may have just hit - `reseek` routes through `seek_bounded`
                // too, so `self.seek_deadline()` is read again from scratch
                // rather than any value carried over from above.
                if let Ok((actual, provenance)) = self.reseek(preserved) {
                    self.position = adopt_preserved(preserved, actual);
                    self.position_provenance = provenance;
                    let _ = self.reinstall(playing);
                }
            }
            Err(error) => {
                // The captured value stands. The decoder is parked at an
                // arbitrary point inside the refinement, so the position has
                // to be re-established explicitly, never assumed.
                self.position = preserved;
                // §5.4: the same fresh-deadline recovery as the cancelled arm
                // above.
                match self.reseek(preserved) {
                    // Cancelled again: the stop or shutdown that cancelled the
                    // seek is about to be handled, and it tears the decoder
                    // down anyway. The preserved position stands.
                    Err(restore_error) if is_cancelled(&restore_error) => {}
                    Ok((actual, provenance)) => {
                        self.position = adopt_preserved(preserved, actual);
                        self.position_provenance = provenance;
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
                    Err(restore_error) => self.fail(format!(
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
            // IMPORTANT 1 (final review): same rule as `restore`'s arm above
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
            match self.reseek(Duration::ZERO) {
                Ok((actual, provenance)) => {
                    self.position = actual;
                    self.position_provenance = provenance;
                    self.pending = None;
                }
                Err(error) if is_cancelled(&error) => return,
                Err(error) => {
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
                    // `reseek`'s own outcome, which for an MP3 restart that
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
            Err(error) => self.fail(format!("cannot start the audio device: {error}")),
        }
    }

    /// A preserving seek: no budget, because it promises to land exactly where
    /// it was told to, but cancellable, so that a stop or a shutdown is not
    /// left waiting for a refinement to finish. A cancelled one leaves the
    /// decoder at an arbitrary point, which is why every caller either
    /// re-establishes it or abandons the operation entirely.
    ///
    /// R2: one of `seek_refined`'s four call sites, routed through
    /// `seek_bounded` - stop→play (`restore`) and device recovery both reach
    /// `seek_refined` only through here. The no-op short-circuit below is
    /// deliberately *not* routed: nothing seeks, so nothing about
    /// `self.position_provenance` should change, and the caller's own
    /// `Ok((target, self.position_provenance))` says exactly that -
    /// carrying it forward unchanged rather than manufacturing a landing
    /// that never happened.
    fn reseek(
        &mut self,
        target: Duration,
    ) -> Result<(Duration, PositionProvenance), PlaybackError> {
        let deadline = self.seek_deadline();
        let Some(source) = self.source.as_mut() else {
            return Ok((target, self.position_provenance));
        };
        if source.position() == target {
            return Ok((target, self.position_provenance));
        }
        seek_bounded(
            source,
            &self.source_interrupt,
            &self.interrupt,
            deadline,
            target,
            None,
        )
        .map(|(outcome, provenance)| (outcome.actual, provenance))
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
    fn retire_remote_source(&mut self) {
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

    fn is_indefinite(&self) -> bool {
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
    fn ensure_source_open(&mut self) -> Result<bool, PlaybackError> {
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
        // MINOR 6 (fix round 1): a stop-then-play cycle reopens against the
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
    /// `Result<bool, PlaybackError>`, not a bare `bool` (IMPORTANT 1, final
    /// review, site 5): the trial goes through `HttpMediaSource::seek`, which
    /// issues a real range request, so a cancellation and a typed remote
    /// failure (a 416, `ResourceChanged`, a stall...) are both reachable here
    /// and neither is the same fact as a demuxer that genuinely refused the
    /// seek. `Ok(false)` is reserved for that last case alone; the caller
    /// tells the three apart rather than reading every failure as "cannot
    /// seek".
    fn verify_seek_support(&mut self) -> Result<bool, PlaybackError> {
        let current = self.position;
        // R2: the fourth of `seek_refined`'s four call sites, routed through
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
fn is_cancelled(error: &PlaybackError) -> bool {
    matches!(error, PlaybackError::Cancelled)
        // A direct `PlaybackError::Remote(RemoteFailure::Cancelled)` - what
        // `prepare`/`HttpMediaSource::open` and `ensure_source_open` produce,
        // never routed through Symphonia at all - is not caught by
        // `remote_cause` below: `#[error(transparent)]` forwards `source()`
        // to the inner `RemoteFailure`'s own `source()`, and `Cancelled` has
        // none, so walking the chain finds nothing (fix round 2, IMPORTANT
        // 1: the reopen-cancellation tests this fixed added are what caught
        // this - every call site that previously used this helper only ever
        // saw a *decode-layer* error Symphonia had mangled a `RemoteIoError`
        // into, which `remote_cause` walks to correctly).
        || matches!(error, PlaybackError::Remote(RemoteFailure::Cancelled))
        || matches!(remote_cause(error), Some(RemoteFailure::Cancelled))
}

/// Whether the interrupt word carries a stop or a shutdown - never the SEEK
/// bit. Every local refinement loop's `cancelled` closure checks this rather
/// than `!= 0`: `submit_seek` sets SEEK for exactly the dispatch that is
/// about to run the seek this closure is guarding, and a bit that means "a
/// seek was accepted" must not read as "abandon the seek that was just
/// accepted." STOP and SHUTDOWN carry no such ambiguity - both mean the
/// operation in progress must give up regardless of what it is.
fn stop_or_shutdown(word: u8) -> bool {
    word & (STOP | SHUTDOWN) != 0
}

/// The one seat every `seek_refined` call goes through (R2 of M3.1 Task 4):
/// `load`, `seek_to`, `reseek` (so `restore`/stop→play and `restart` reach it
/// too) and `verify_seek_support` all call this rather than `DecodedSource::
/// seek_refined` directly, so the deadline policy below cannot drift between
/// them.
///
/// A free function rather than a `Worker` method: `load` calls this before
/// `decoded` is installed as `self.source`, so `source` has to be a
/// standalone parameter the caller already holds, and threading `self`
/// through as well would force every caller to give up the disjoint
/// `self.source`/`self.source_interrupt`/`self.interrupt` borrows the
/// existing call sites already rely on.
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
/// all (fix round 1). So the provenance this returns is **not** asserted
/// here: it is read straight off `SeekOutcome::provenance`, which
/// `seek_refined` itself computes from the reader's own `format_info()` -
/// `Estimated` for MP3, `Established` for every other format this crate
/// supports, exactly as it always reported before this task, never inferred
/// from the target, a Xing/Info tag, or which of the four callers reached
/// it. A caller that must not manufacture a landing that never happened
/// (`reseek`'s own no-op short-circuit, already at target) is the one place
/// that deliberately does not call this at all - see its own comment.
fn seek_bounded(
    source: &mut DecodedSource,
    source_interrupt: &SourceInterrupt,
    local_interrupt: &AtomicU8,
    deadline: Duration,
    target: Duration,
    budget: Option<Duration>,
) -> Result<(SeekOutcome, PositionProvenance), PlaybackError> {
    source_interrupt.set_operation_deadline(Some(Instant::now() + deadline));
    let outcome = source.seek_refined(target, budget, &mut || {
        stop_or_shutdown(local_interrupt.load(Ordering::Acquire))
    });
    source_interrupt.set_operation_deadline(None);
    // Provenance comes from `SeekOutcome` itself, not asserted here: it
    // follows the demuxer that actually ran `Coarse` (MP3 only - fix round
    // 1), never asserted uniformly for every landing regardless of format.
    outcome.map(|outcome| {
        let provenance = outcome.provenance;
        (outcome, provenance)
    })
}

fn severity(fault: OutputFault) -> u8 {
    match fault {
        OutputFault::Recoverable(_) => 0,
        OutputFault::Rebuild(_) => 1,
        OutputFault::Fatal(_) => 2,
    }
}

/// Retire every fault belonging to a transport being torn down.
///
/// A fault describes the transport that produced it, so carrying one past a
/// teardown lets an obsolete fatal fault fire later and turn a completed `Stop`
/// into `Failed`. Both sources have to go: the one held back for want of event
/// room, and anything still queued in the channel — an interrupt arriving
/// before the queue was drained leaves the fault there rather than deferred.
fn retire_faults(deferred: &mut Option<OutputFault>, faults: &Receiver<OutputFault>) {
    *deferred = None;
    while faults.try_recv().is_ok() {}
}

fn frames_to_duration(frames: u64, rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(rate.max(1)))
}

/// Best-effort text for a `std::thread::JoinHandle::join` panic payload. Panic
/// payloads are almost always a `&'static str` or a `String`; anything else
/// reports as opaque rather than being dropped silently.
fn describe_panic(panic: &(dyn Any + Send)) -> String {
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

/// The oldest event `Worker::emit` may displace to make room for a terminal
/// arrival at `PENDING_CAP`: neither terminal (already guaranteed delivery
/// some other way) nor protected (never displaced — Decision 4). A free
/// function so the unit test below can exercise it directly.
fn displacement_victim(pending: &VecDeque<PlaybackEvent>) -> Option<usize> {
    pending
        .iter()
        .position(|event| !event.is_terminal() && !event.is_protected())
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
    use super::*;

    /// A terminal, non-protected event may be displaced to make room for
    /// another terminal event at `PENDING_CAP`; a protected one — a load
    /// outcome — never may, since correlation cannot be inferred from
    /// anything later (Decision 4).
    #[test]
    fn a_terminal_event_never_displaces_a_protected_one() {
        let mut pending = VecDeque::new();
        pending.push_back(PlaybackEvent::LoadCancelled {
            session_rev: 1,
            request: LoadRequestId::from_raw(1),
        });
        pending.push_back(PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        });
        pending.push_back(PlaybackEvent::Warning {
            session_rev: 1,
            message: String::new(),
        });
        assert_eq!(displacement_victim(&pending), Some(2));
        pending.pop_back();
        assert_eq!(displacement_victim(&pending), None);
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
}
