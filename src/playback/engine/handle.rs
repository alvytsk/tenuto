//! The front-of-house engine handle: admissions, interrupts, join.

//! The decode worker: the state machine that owns every pipeline component.
//!
//! The invariant this module exists to enforce is that stopping or recreating
//! the audio pipeline never implicitly resets the logical playback position.
//! Position lives here, in `Worker::position`, and only four things establish a
//! new one: loading media, an explicit restart, a successful seek, and a resume
//! that adopts a target stored while stopped. Everything else — pausing,
//! stopping, a device disappearing, a handshake timing out — preserves it.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::clock::{Clock, SystemClock};
use crate::http::channel::SourceInterrupt;
use crate::http::limits::Limits;
use crate::http::service::HttpService;

use super::super::command::{Admission, PlaybackCommand};
use super::super::event::{PlaybackEvent, Progress, ShutdownReport};
use super::super::lock;
use super::super::outbox::{self};
use super::super::output::AudioOutput;
use super::super::output::cpal_output::{CpalOutput, OutputFault};
use super::super::output::null_output::NullOutput;
use super::super::reconnect::ReconnectPolicy;
use super::super::spectrum::worker::{self as spectrum_worker, SpectrumHandle, SpectrumThread};
use super::super::timeline::PositionQuality;
use crate::media::provenance::PositionProvenance;

use super::worker::describe_panic;
use super::{COMMAND_CAPACITY, SHUTDOWN, STOP, Worker};

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
    /// A finite recovery is in progress (M10 §7): `submit_pause` must retire
    /// the attempt's read, not freeze it, as for a station.
    pub recovering: AtomicBool,
    /// `Playing` or `Reconnecting`: the states a `TogglePause` pauses.
    pub pausable: AtomicBool,
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
    /// there is nothing to swap). `submit` acts on it directly, out of band from
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
        Self::assemble(
            Box::new(output),
            faults,
            wake_tx,
            wake_rx,
            Arc::new(SystemClock),
        )
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
        Self::spawn_on_clock(output, faults, Arc::new(SystemClock))
    }

    /// [`Self::spawn_with`], with the network clock every stall, header,
    /// seek and opening budget and every reconnect backoff is measured on
    /// (M9.5). A test passes a `FakeClock` and steps it past a budget.
    pub fn spawn_on_clock(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(64);
        Self::assemble(output, faults, wake_tx, wake_rx, clock)
    }

    fn assemble(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        wake_tx: Sender<()>,
        wake_rx: Receiver<()>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (events_out, events_rx) = outbox::channel();
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
        let source_interrupt = SourceInterrupt::with_clock(Limits::default().buffer_bytes, clock);
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
            events_out,
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

    /// The raw queue, with none of [`Self::submit`]'s out-of-band rules: a
    /// command sent here reaches the worker only when it next looks. A test
    /// seam for driving the worker's dispatch alone; production code goes
    /// through `submit` (`tests/m9_4_submission_door.rs` checks it).
    #[doc(hidden)]
    pub fn commands(&self) -> &Sender<PlaybackCommand> {
        &self.commands
    }

    pub fn events(&self) -> &Receiver<PlaybackEvent> {
        &self.stream.events
    }

    /// The wake channel: a device backend pings it so a fault interrupts a
    /// blocked wait rather than waiting out the tick. A test seam, like
    /// [`Self::commands`].
    #[doc(hidden)]
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
        lock(&self.progress).clone()
    }

    /// Out-of-band stop: travels on the interrupt flag plus the wake channel,
    /// so a saturated event channel cannot delay it. Also retires the source
    /// interrupt: a stop must reach a worker blocked inside a remote read,
    /// not only one waiting on the command channel or the tick.
    fn interrupt_stop(&self) {
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
    fn interrupt_shutdown(&self) {
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

    /// The one door (M9.4): every command, with whatever out-of-band rule it
    /// needs to reach a worker blocked in a source read. §8: queue admission
    /// and waking belong together, so no caller can queue a command and
    /// forget to wake, retire, freeze or thaw anything.
    ///
    /// `Stop` and `Shutdown` never queue: they travel on the interrupt word,
    /// so a saturated queue or a running refinement cannot delay them, and
    /// they are always `Accepted`. `TogglePause` is resolved here, against
    /// the worker's own published state, because only the direction chosen
    /// knows whether to freeze or thaw.
    pub fn submit(&self, command: PlaybackCommand) -> Admission {
        match command {
            PlaybackCommand::Stop => {
                self.interrupt_stop();
                Admission::Accepted
            }
            PlaybackCommand::Shutdown => {
                self.interrupt_shutdown();
                Admission::Accepted
            }
            PlaybackCommand::TogglePause => {
                if self.traits.pausable.load(Ordering::Acquire) {
                    self.submit_pause()
                } else {
                    self.submit_play()
                }
            }
            PlaybackCommand::Pause => self.submit_pause(),
            PlaybackCommand::Play => self.submit_play(),
            PlaybackCommand::SeekTo(_) | PlaybackCommand::SeekBy(_) => self.submit_seek(command),
            PlaybackCommand::Load { .. } => {
                // A load tears the current source down whatever it is, so its
                // blocked read or open is woken now rather than waited out
                // (M7 §6.1). Aimed at the generation observed *before* the
                // send, like a seek.
                let generation = self.source_interrupt.generation();
                let admission = self.try_send(command);
                if admission == Admission::Accepted {
                    self.source_interrupt.retire_generation(generation);
                    let _ = self.wake.try_send(());
                }
                admission
            }
            other => {
                let admission = self.try_send(other);
                if admission == Admission::Accepted {
                    let _ = self.wake.try_send(());
                }
                admission
            }
        }
    }

    /// A seek, published to the source interrupt only after the queue
    /// accepts it (§8): a seek refused admission must not retire a fetch it
    /// never got to replace.
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
    fn submit_seek(&self, command: PlaybackCommand) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(command);
        if admission != Admission::Accepted {
            return admission;
        }
        // A source that cannot seek is going to answer `SeekRejected`; retiring
        // it first would cost the listener the stream for nothing (M7 §3.4).
        if !self.traits.seek_unsupported.load(Ordering::Acquire) {
            self.source_interrupt.retire_generation(generation);
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
    /// a seek — a superseded generation means the worker is no longer
    /// blocked in anything this had to wake. A finite recovery is retired too
    /// (M10 §7): a frozen reopen or priming read would also never wake, and a
    /// parked half-built transport would announce `Paused` mid-attempt.
    fn submit_pause(&self) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(PlaybackCommand::Pause);
        if admission == Admission::Accepted {
            if self.traits.indefinite.load(Ordering::Acquire)
                || self.traits.recovering.load(Ordering::Acquire)
            {
                self.source_interrupt.retire_generation(generation);
            } else {
                self.source_interrupt.freeze();
            }
            let _ = self.wake.try_send(());
        }
        admission
    }

    /// `Play`, plus the level's release.
    fn submit_play(&self) -> Admission {
        let admission = self.try_send(PlaybackCommand::Play);
        if admission == Admission::Accepted {
            self.source_interrupt.thaw();
            let _ = self.wake.try_send(());
        }
        admission
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
