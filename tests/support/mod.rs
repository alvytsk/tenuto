//! Harness for the engine contract tests.
//!
//! The engine runs on its own thread against a virtual device, so the tests
//! control time explicitly. The driver thread has two modes:
//!
//! * *frozen* — the clock does not move, and the callback runs only while a
//!   handshake is in flight (any phase but `Run`). Transitions still complete,
//!   but no audio is consumed and no instant passes, so a position read before
//!   a command and one read after it are comparable exactly.
//! * *advancing* — one buffer period of virtual time per step, which is what
//!   playing audio looks like. Used only to reach the end of a track, where
//!   how much time passes on the way does not matter.
//!
//! Anything that has to stop at a particular point instead steps the clock from
//! the test thread — `play_for`, `let_time_pass` — so that how far playback ran
//! is a count this harness kept rather than a consequence of how long the
//! scheduler kept the test thread away.
//!
//! Every assertion about preservation is made with the clock frozen; that is
//! what makes `assert_eq!` on a position honest rather than flaky.

#![allow(dead_code)]

pub mod browse;
pub mod server;
pub mod wav;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use url::Url;

use tenuto::clock::{Clock, FakeClock, SystemClock};
use tenuto::http::channel::{SourceInterrupt, WaitHook};
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::http::source::{HttpMediaSource, OpeningDeadline};
use tenuto::media::capabilities::MediaCapabilities;
use tenuto::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::playback::callback::CallbackCore;
use tenuto::playback::command::{Admission, LoadRequestId, PlaybackCommand, ResumeIntent};
use tenuto::playback::engine::EngineHandle;
use tenuto::playback::error::PlaybackError;
use tenuto::playback::event::{PlaybackEvent, Progress, ShutdownReport, StartDisposition};
use tenuto::playback::link::{OutputLink, Phase};
use tenuto::playback::output::cpal_output::OutputFault;
use tenuto::playback::output::test_output::TestOutput;
use tenuto::playback::output::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::volume::Volume;

const CHANNELS: u16 = 2;
const RATE: u32 = 48_000;
/// 2 ms at 48 kHz: one period, and the granularity of the virtual clock.
const BUFFER_FRAMES: u32 = 96;
const PERIOD: Duration = Duration::from_millis(2);
/// Deliberately generous, so that "the ring is empty" and "the last frame has
/// been heard" are far apart in time and the end-of-track rule is observable.
const LATENCY: Duration = Duration::from_millis(100);
/// Frames the virtual device has rendered but not played when the worker
/// freezes it with the clock stopped: one output latency of buffers, each
/// rendered a latency before it plays. A capture counts them unheard, so a
/// teardown that resumes at the captured position renders them again. A
/// freeze answered while the clock advances moves it one period first, and
/// one period fewer is in flight.
pub const UNHEARD_FRAMES: u32 = LATENCY.as_millis() as u32 * RATE / 1000;
const DRIVER_NAP: Duration = Duration::from_micros(500);
/// Between two periods of a drain that is still producing audio.
const PACING_NAP: Duration = Duration::from_millis(1);
/// Between two periods of a drain that has gone silent. Long enough that a
/// worker thread waiting behind a full CPU has been scheduled and has had its
/// chance to refill the ring, so silence means the ring is empty rather than
/// that the machine was busy.
const SILENCE_GRACE: Duration = Duration::from_millis(25);
/// How far the harness clock may run ahead of the playback it drives, over and
/// above the one output latency the pipeline owes it. Ten periods: the engine's
/// span ring holds 64 records and the callback writes one per period, so this
/// keeps the backlog an order of magnitude short of losing a span.
const CLOCK_LEAD_SLACK: Duration = Duration::from_millis(20);
const PATIENCE: Duration = Duration::from_secs(20);

const FROZEN: u8 = 0;
const ADVANCING: u8 = 1;

#[allow(clippy::unwrap_used)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned harness mutex means a test thread already panicked; there is
    // nothing better to do than propagate it.
    mutex.lock().unwrap()
}

/// Public so a test can build a path to hand `TestEngine::load_with_resume`
/// directly, for a load whose resume intent `start`/`start_at`'s own
/// `ResumeIntent::StartAt` cannot express.
#[allow(clippy::unwrap_used)]
pub fn fixture(name: &str) -> AbsolutePath {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    AbsolutePath::new(path.canonicalize().unwrap()).unwrap()
}

/// The path of a fixture, for tests that need its bytes rather than an
/// `AbsolutePath` - `TestServer::start(Script::serving(...))` among them. A
/// bare helper, so it panics with the path on failure: a missing fixture is
/// a repository error, not a test condition.
pub fn fixture_path(name: &str) -> std::path::PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    path.canonicalize()
        .unwrap_or_else(|error| panic!("fixture {path:?} must exist: {error}"))
}

/// A `MediaId` for a local file that need not exist, for tests that only care
/// about identity. Shared across the persistence and session test files
/// rather than duplicated in each.
pub fn media(name: &str) -> MediaId {
    // A bare helper, so it handles its own error: the lint exemption stops at
    // the `#[test]` boundary.
    match AbsolutePath::new(format!("/music/{name}.flac").into()) {
        Ok(path) => MediaId::LocalFile(path),
        Err(error) => panic!("a literal absolute path must parse: {error}"),
    }
}

/// A `WaitHook` that does nothing, for a harness open that never needs to
/// service anything while waiting.
struct NoopHook;
impl WaitHook for NoopHook {
    fn service(&self) {}
}

/// Open `/radio` on `server` at the HTTP seam, bypassing the engine, and
/// finish opening immediately.
///
/// Shared by `tests/m7_http_live.rs` and `tests/m7_1_no_icy_metadata.rs`, both
/// of which only need the request the open makes, not a playing engine.
pub fn open_station(server: &server::TestServer) -> HttpMediaSource {
    let limits = Limits::brisk();
    let service = HttpService::spawn(limits).unwrap_or_else(|error| panic!("service: {error}"));
    let url = Url::parse(&server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let interrupt = SourceInterrupt::new(limits.buffer_bytes);
    let (source, opening) = HttpMediaSource::open(
        service,
        url,
        interrupt,
        Arc::new(NoopHook),
        limits,
        OpeningDeadline(Instant::now() + limits.open),
    )
    .unwrap_or_else(|error| panic!("open: {error}"));
    opening.finish_opening();
    source
}

struct Device {
    output: TestOutput,
    /// Stashed on every `open`, so the harness can reach the counters the
    /// callback writes without the engine exposing its internals.
    link: Option<Arc<OutputLink>>,
}

struct HarnessOutput {
    device: Arc<Mutex<Device>>,
    /// Counts calls to `negotiate` - the first step of every open attempt -
    /// so a test can prove a later command did not reopen a device that
    /// already failed (M5 §6). `Arc`-shared rather than plain, so a test can
    /// keep reading it after this harness has moved into the engine thread.
    negotiations: Arc<AtomicUsize>,
}

impl HarnessOutput {
    fn new(device: Arc<Mutex<Device>>) -> Self {
        Self {
            device,
            negotiations: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl AudioOutput for HarnessOutput {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError> {
        self.negotiations.fetch_add(1, Ordering::Relaxed);
        lock(&self.device).output.negotiate(request)
    }

    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        core: CallbackCore,
    ) -> Result<(), PlaybackError> {
        let mut device = lock(&self.device);
        device.link = Some(Arc::clone(&link));
        device.output.open(config, link, core)
    }

    fn now(&self) -> Nanos {
        lock(&self.device).output.now()
    }

    fn close(&mut self) {
        let mut device = lock(&self.device);
        device.output.close();
        device.link = None;
    }
}

struct Driver {
    device: Arc<Mutex<Device>>,
    mode: AtomicU8,
    /// When set, the callback is not run while the worker is waiting for a
    /// `Discard` to be acknowledged. Every other phase is answered normally,
    /// which is what separates this from `silence_the_device`: the recovery
    /// the timeout drops into still gets a live device to capture from, which
    /// is the only condition under which a stale timeline can be misread.
    deaf_to_discard: AtomicBool,
    stop: AtomicBool,
    /// The fake network clock, when there is one, and whether it follows
    /// real time (`run_network`) or holds still until stepped.
    network: Option<Arc<FakeClock>>,
    network_runs: AtomicBool,
}

impl Driver {
    fn step(&self) {
        let mut device = lock(&self.device);
        let phase = device.link.as_ref().map(|link| link.load_control().phase);
        if self.deaf_to_discard.load(Ordering::Relaxed) && phase == Some(Phase::Discard) {
            return;
        }
        if self.mode.load(Ordering::Relaxed) == ADVANCING {
            device.output.advance(PERIOD);
            return;
        }
        // Frozen: only run the callback while a transition needs answering.
        // Running it in `Run` would replay a buffer at an instant that has
        // already been used, which is not something a device ever does.
        let running = device
            .link
            .as_ref()
            .is_some_and(|link| link.load_control().phase == Phase::Run);
        if !running && device.link.is_some() {
            device.output.pump_in_place();
        }
    }
}

/// The fields of a `Loaded` event a resume test cares about, from
/// `TestEngine::await_loaded`.
pub struct Loaded {
    pub position: Duration,
    pub disposition: StartDisposition,
    pub capabilities: MediaCapabilities,
}

/// Where a `SeekCompleted` landed, and its provenance (§3), from
/// `TestEngine::await_seek_completed`.
///
/// `PartialEq<Duration>`/`PartialOrd<Duration>` are implemented by hand,
/// rather than deriving them against `Self`, so every existing call site
/// that compares the old `Duration`-only return value (`landed >=
/// Duration::from_secs(2)`, `{landed:?}`) keeps compiling unchanged; a test
/// that cares about provenance reads `.provenance` explicitly instead.
#[derive(Clone, Copy, Debug)]
pub struct SeekLanding {
    pub actual: Duration,
    pub provenance: PositionProvenance,
}

impl PartialEq<Duration> for SeekLanding {
    fn eq(&self, other: &Duration) -> bool {
        self.actual == *other
    }
}

impl PartialOrd<Duration> for SeekLanding {
    fn partial_cmp(&self, other: &Duration) -> Option<std::cmp::Ordering> {
        self.actual.partial_cmp(other)
    }
}

pub struct TestEngine {
    handle: Mutex<Option<EngineHandle>>,
    commands: Sender<PlaybackCommand>,
    faults: Sender<OutputFault>,
    wake: Sender<()>,
    device: Arc<Mutex<Device>>,
    driver: Arc<Driver>,
    thread: Mutex<Option<JoinHandle<()>>>,
    inbox: Mutex<Vec<PlaybackEvent>>,
    states: Mutex<Vec<PlaybackState>>,
    /// How far `await_state` has consumed the state history. A state the
    /// engine has already passed through must not satisfy a later wait.
    consumed_states: Mutex<usize>,
    draining: AtomicBool,
    /// Built lazily by `load_remote`'s first call, then kept for the
    /// engine's lifetime.
    http: Mutex<Option<Arc<HttpService>>>,
    /// Whether an `EndOfTrack` was ever observed in this run. Latched
    /// rather than derived from `inbox`, since `play_until_terminal` and
    /// other draining helpers are free to consume events out of a test's
    /// direct sight.
    saw_end_of_track: AtomicBool,
    /// The token the next `Load` a helper sends (rather than an explicit
    /// `*_as` caller) draws from. Sequential from 1, so a test asserting
    /// `r.get() == 1` against the very first load a bare helper method sends
    /// keeps working unchanged.
    next_request: AtomicU64,
}

/// A live borrow of the engine's `EngineHandle`, returned by `handle()`.
/// See that method's doc comment for why this exists rather than a bare
/// `&EngineHandle`.
pub struct HandleRef<'a>(MutexGuard<'a, Option<EngineHandle>>);

impl std::ops::Deref for HandleRef<'_> {
    type Target = EngineHandle;

    fn deref(&self) -> &EngineHandle {
        match self.0.as_ref() {
            Some(handle) => handle,
            None => panic!("the engine handle is gone; the test outlived a shutdown"),
        }
    }
}

impl TestEngine {
    pub fn start(name: &str) -> Self {
        let engine = Self::start_at(name, Duration::ZERO);
        // The events a start-up emits are not what any test is looking at.
        lock(&engine.inbox).clear();
        engine
    }

    /// A start that resumes at `start_at`. Deliberately does **not** clear the
    /// inbox: the `Loaded` it produces is the subject of the resume tests.
    pub fn start_at(name: &str, start_at: Duration) -> Self {
        let mut engine = Self::bare();
        engine.load_with_resume(fixture(name), ResumeIntent::StartAt(start_at));
        engine.send(PlaybackCommand::Play);
        engine.await_state(PlaybackState::Playing);
        engine
    }

    /// The device and worker wired up, nothing loaded - `Idle`, ready for
    /// `load_remote`. `start`/`start_at` need a local fixture immediately;
    /// a remote test needs control over exactly when the load happens (and
    /// needs to install an `HttpService` first), so it starts here rather
    /// than through either of them. Rust has no argument-count overloading,
    /// so this cannot be a zero-argument `start()` alongside `start(name)`.
    pub fn start_idle() -> Self {
        Self::bare()
    }

    /// [`start_idle`](Self::start_idle) with a zero reconnect budget: the
    /// first drop of a remote episode fails the session, as it did before
    /// M10 recovery. For tests about what a failure is, not how it heals.
    pub fn start_without_recovery() -> Self {
        let engine = Self::bare();
        engine.handle().set_reconnect_policy(ReconnectPolicy {
            budget: Duration::ZERO,
            ..ReconnectPolicy::default()
        });
        engine
    }

    /// [`start_idle`](Self::start_idle) on a held network clock (M9.5): no
    /// stall, header or seek budget expires and no backoff elapses until the
    /// test steps it ([`advance_network`](Self::advance_network)) or lets it
    /// follow real time ([`run_network`](Self::run_network)). A test acting
    /// inside a backoff then has all the time it needs, however loaded the
    /// machine.
    pub fn start_on_fake_clock() -> Self {
        Self::bare_on(Some(Arc::new(FakeClock::new())))
    }

    /// Step the fake network clock. Waits and the worker loop re-read it
    /// within one slice.
    pub fn advance_network(&self, span: Duration) {
        self.network().advance_monotonic(span);
    }

    /// From now on the network clock follows real time, as a real one would.
    pub fn run_network(&self) {
        self.network();
        self.driver.network_runs.store(true, Ordering::Relaxed);
    }

    /// Hold the network clock where it is.
    pub fn hold_network(&self) {
        self.network();
        self.driver.network_runs.store(false, Ordering::Relaxed);
    }

    fn network(&self) -> &FakeClock {
        match &self.driver.network {
            Some(clock) => clock,
            None => panic!("the network clock is real; use start_on_fake_clock"),
        }
    }

    fn bare() -> Self {
        Self::bare_on(None)
    }

    fn bare_on(network: Option<Arc<FakeClock>>) -> Self {
        let device = Arc::new(Mutex::new(Device {
            output: TestOutput::new(CHANNELS, RATE, BUFFER_FRAMES, LATENCY),
            link: None,
        }));
        let (fault_tx, fault_rx) = crossbeam_channel::bounded(16);
        let clock: Arc<dyn Clock> = match &network {
            Some(fake) => Arc::clone(fake) as Arc<dyn Clock>,
            None => Arc::new(SystemClock),
        };
        let handle = EngineHandle::spawn_on_clock(
            Box::new(HarnessOutput::new(Arc::clone(&device))),
            fault_rx,
            clock,
        );
        let driver = Arc::new(Driver {
            device: Arc::clone(&device),
            mode: AtomicU8::new(FROZEN),
            deaf_to_discard: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            network,
            network_runs: AtomicBool::new(false),
        });
        let thread = {
            let driver = Arc::clone(&driver);
            std::thread::Builder::new()
                .name("harness-device".into())
                .spawn(move || {
                    let mut last = Instant::now();
                    while !driver.stop.load(Ordering::Relaxed) {
                        driver.step();
                        std::thread::sleep(DRIVER_NAP);
                        let elapsed = last.elapsed();
                        last += elapsed;
                        if let Some(clock) = &driver.network
                            && driver.network_runs.load(Ordering::Relaxed)
                        {
                            clock.advance_monotonic(elapsed);
                        }
                    }
                })
                .ok()
        };
        Self {
            commands: handle.commands().clone(),
            wake: handle.wake().clone(),
            handle: Mutex::new(Some(handle)),
            faults: fault_tx,
            device,
            driver,
            thread: Mutex::new(thread),
            inbox: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
            consumed_states: Mutex::new(0),
            draining: AtomicBool::new(true),
            http: Mutex::new(None),
            saw_end_of_track: AtomicBool::new(false),
            next_request: AtomicU64::new(1),
        }
    }

    /// The token the next bare (non-`*_as`) `Load`-sending helper will use.
    /// Sequential from 1, allocated here so every such helper draws from one
    /// counter rather than each hardcoding its own.
    pub fn next_request(&self) -> LoadRequestId {
        LoadRequestId::from_raw(self.next_request.fetch_add(1, Ordering::Relaxed))
    }

    /// Load an HTTP source. Builds an `HttpService` on first use (`Limits`
    /// short enough that the cancellation tests do not spend real seconds
    /// waiting on a deadline they intend to hit) and keeps it for the
    /// engine's lifetime; every later `load_remote` on this `TestEngine`
    /// reuses it.
    pub fn load_remote(&mut self, url: &str) {
        let request = self.next_request();
        self.load_remote_inner(
            request,
            url,
            ResumeIntent::StartAt(Duration::ZERO),
            None,
            true,
        );
    }

    /// `load_remote` under a caller-decided `ResumeIntent`, for a resume test
    /// whose second session needs `Candidate` rather than the fixed
    /// `StartAt(ZERO)` `load_remote` always sends (H4, H5's protected
    /// fallback). Shares the cached brisk `HttpService`, same as
    /// `load_remote`.
    pub fn load_remote_with_resume(&mut self, url: &str, resume: ResumeIntent) {
        let request = self.next_request();
        self.load_remote_inner(request, url, resume, None, true);
    }

    /// `load_remote` against a dedicated `HttpService` built from `limits`
    /// rather than the cached brisk one — H13's starvation half needs a
    /// `stall` deadline generous enough that draining the ring and reading
    /// the frozen position afterwards cannot itself race the brisk 500 ms
    /// one into a spurious `Failed`.
    ///
    /// Hands back the token it allocated, so a caller that needs both custom
    /// limits and a `PlayLoaded` for this very load can name it. Ignoring the
    /// return value is the common case.
    pub fn load_remote_with_limits(&mut self, url: &str, limits: Limits) -> LoadRequestId {
        let request = self.next_request();
        self.load_remote_inner(
            request,
            url,
            ResumeIntent::StartAt(Duration::ZERO),
            Some(limits),
            true,
        );
        request
    }

    /// `load_remote_with_limits` under a caller-decided `ResumeIntent`: M10's
    /// cancellation tests need both a proven (resumed) episode and deadlines
    /// only a command can beat.
    pub fn load_remote_with_resume_and_limits(
        &mut self,
        url: &str,
        resume: ResumeIntent,
        limits: Limits,
    ) -> LoadRequestId {
        let request = self.next_request();
        self.load_remote_inner(request, url, resume, Some(limits), true);
        request
    }

    /// `load_remote`, but for a load this test expects to fail rather than
    /// reach `Paused` (§12's closing paragraph: a sequential-only source
    /// opening a tail-`moov` file). The `HttpService` still has to be
    /// attached for the attempt to mean anything — without one the load
    /// fails immediately as "no HTTP service", which would prove nothing
    /// about the file itself.
    pub fn load_remote_expecting_failure(&mut self, url: &str) {
        let request = self.next_request();
        self.load_remote_inner(
            request,
            url,
            ResumeIntent::StartAt(Duration::ZERO),
            None,
            false,
        );
    }

    /// `load_remote_with_resume` under a caller-chosen token, for a test that
    /// must correlate this load's events against a request it names itself
    /// rather than the counter's next value.
    pub fn load_remote_as(&mut self, request: LoadRequestId, url: &str, resume: ResumeIntent) {
        self.load_remote_inner(request, url, resume, None, true);
    }

    /// Shared body for the `load_remote*` entry points above. `limits`:
    /// `None` reuses (and lazily populates) the cached brisk service every
    /// plain `load_remote` shares; `Some` always spawns a fresh service
    /// under those limits and replaces the cached one with it, which is fine
    /// because every test that asks for custom limits loads exactly once.
    /// `await_paused`: false for a load this test expects to fail, so it
    /// does not wait for a state the attempt is never going to reach.
    fn load_remote_inner(
        &mut self,
        request: LoadRequestId,
        url: &str,
        resume: ResumeIntent,
        limits: Option<Limits>,
        await_paused: bool,
    ) {
        let service = match limits {
            Some(limits) => {
                let service = match HttpService::spawn(limits) {
                    Ok(service) => service,
                    Err(error) => panic!("the test HttpService must start: {error}"),
                };
                *lock(&self.http) = Some(Arc::clone(&service));
                service
            }
            None => {
                let mut http = lock(&self.http);
                if http.is_none() {
                    let service = match HttpService::spawn(Limits::brisk()) {
                        Ok(service) => service,
                        Err(error) => panic!("the test HttpService must start: {error}"),
                    };
                    *http = Some(service);
                }
                #[allow(clippy::unwrap_used)] // just populated above if it was empty.
                http.clone().unwrap()
            }
        };
        if let Some(handle) = lock(&self.handle).as_ref() {
            handle.set_http(Some(service));
        }
        let parsed = match Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => panic!("test URL {url:?} must parse: {error}"),
        };
        let media = match NormalizedUrl::parse(url) {
            Ok(normalized) => MediaId::RemoteUrl(normalized),
            Err(error) => panic!("test URL {url:?} must normalize: {error}"),
        };
        // Through `submit`, not the bare command channel: a remote `Load` is
        // one of the submissions that acts on the source interrupt out of
        // band (M7 §6.1 - it retires the source it is about to replace), so
        // a harness that side-steps `submit` would exercise a path the
        // application never takes.
        let admission = self.handle().submit(PlaybackCommand::Load {
            request,
            media,
            source: SourceLocation::Http(parsed),
            resume,
        });
        assert_eq!(
            admission,
            Admission::Accepted,
            "the engine must accept the load"
        );
        if await_paused {
            self.await_state(PlaybackState::Paused);
        }
    }

    /// The handle, for the submission methods (`submit_pause`, `submit_seek`
    /// …). A thin `Deref<Target = EngineHandle>` wrapper around a lock guard,
    /// not a bare `&EngineHandle`: the handle lives behind the same `Mutex`
    /// `drop_event_receiver` and shutdown already share (a `TestEngine` bound
    /// without `mut`, as `a_disconnected_event_receiver_terminates_the_worker`
    /// does, still has to be able to call `drop_event_receiver`), so nothing
    /// here can hand back a bare reference that outlives the guard reading
    /// it. `engine.handle().submit_pause()` reads exactly as if it had.
    pub fn handle(&self) -> HandleRef<'_> {
        HandleRef(lock(&self.handle))
    }

    // ------------------------------------------------------------- commands

    pub fn send(&mut self, command: PlaybackCommand) {
        if self.commands.send(command).is_err() {
            panic!("the engine stopped accepting commands");
        }
    }

    /// Send `Load` for `path` under a caller-decided resume intent, and wait
    /// for the source to open. `start_at` sends its own initial load through
    /// here as `ResumeIntent::StartAt`; a test that needs a
    /// `ResumeIntent::Candidate` - one only the worker's own decode probe can
    /// resolve - calls this directly, loading a second time under an intent
    /// `start`/`start_at` cannot express.
    pub fn load_with_resume(&mut self, path: AbsolutePath, resume: ResumeIntent) {
        let request = self.next_request();
        self.load_with_resume_as(request, path, resume);
    }

    /// `load_with_resume` under a caller-chosen token, for a test that must
    /// correlate this load's events against a request it names itself rather
    /// than the counter's next value.
    pub fn load_with_resume_as(
        &mut self,
        request: LoadRequestId,
        path: AbsolutePath,
        resume: ResumeIntent,
    ) {
        self.send(PlaybackCommand::Load {
            request,
            media: MediaId::LocalFile(path.clone()),
            source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
            resume,
        });
        self.await_state(PlaybackState::Paused);
    }

    pub fn interrupt_stop(&mut self) {
        if let Some(handle) = lock(&self.handle).as_ref() {
            handle.interrupt_stop();
        }
    }

    /// Inject a fatal device fault, the kind that ends a session.
    pub fn force_fatal_device_fault(&mut self) {
        let _ = self
            .faults
            .send(OutputFault::Fatal(cpal::ErrorKind::PermissionDenied));
        let _ = self.wake.try_send(());
    }

    /// Inject the device fault a vanished output device reports.
    pub fn force_device_loss(&mut self) {
        let _ = self
            .faults
            .send(OutputFault::Rebuild(cpal::ErrorKind::DeviceNotAvailable));
        let _ = self.wake.try_send(());
    }

    /// Answer every handshake phase except `Discard`, which is left to run to
    /// its deadline. Reversible, so the recovery the timeout triggers can
    /// complete against a device that works again.
    pub fn stop_answering_discards(&mut self) {
        self.driver.deaf_to_discard.store(true, Ordering::Relaxed);
    }

    pub fn answer_discards_again(&mut self) {
        self.driver.deaf_to_discard.store(false, Ordering::Relaxed);
    }

    /// Kill the device: it accepts everything and answers nothing, so every
    /// handshake wait runs to its deadline. Opt-in and permanent, so a recovery
    /// that reopens the device still meets a dead one.
    pub fn silence_the_device(&mut self) {
        lock(&self.device).output.stop_responding();
    }

    /// Wait until the worker has published `Freeze`, which is the first thing a
    /// recovery does. Polling the published phase rather than sleeping a guessed
    /// interval is what keeps the cancellation test deterministic: the freeze
    /// then sits on its deadline for as long as the test needs.
    pub fn await_recovery_capture(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            {
                let device = lock(&self.device);
                if device
                    .link
                    .as_ref()
                    .is_some_and(|link| link.load_control().phase == Phase::Freeze)
                {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!("the worker never began capturing for a recovery");
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Whether the device's most recently captured buffer holds any nonzero
    /// sample — proof that real audio, not silence, reached the output. H1:
    /// playback must be audible while a remote body is still arriving, not
    /// merely "not failed".
    pub fn captured_is_audible(&self) -> bool {
        lock(&self.device)
            .output
            .captured()
            .iter()
            .any(|sample| *sample != 0.0)
    }

    /// Every sample the virtual device rendered since the last
    /// `clear_rendered`, interleaved. `TestOutput` already keeps the whole
    /// log; this only hands it out.
    pub fn rendered(&self) -> Vec<f32> {
        lock(&self.device).output.captured().to_vec()
    }

    pub fn clear_rendered(&mut self) {
        lock(&self.device).output.clear_captured();
    }

    pub fn inject_xruns(&mut self, count: usize) {
        let device = lock(&self.device);
        let Some(link) = device.link.as_ref() else {
            panic!("no transport is open, so there is nothing to inject into");
        };
        for _ in 0..count {
            link.note_xrun();
        }
    }

    /// Commands the worker has not taken off the channel yet. Admission
    /// closing is otherwise invisible from outside.
    pub fn pending_commands(&mut self) -> usize {
        lock(&self.handle)
            .as_ref()
            .map_or(0, |handle| handle.commands().len())
    }

    pub fn drop_event_receiver(&self) {
        if let Some(handle) = lock(&self.handle).as_mut() {
            handle.release_events();
        }
    }

    /// Interrupt and join, handing back what the engine captured on its way
    /// out. `Drop` then finds the handle already taken and skips its own join.
    pub fn shutdown_report(&mut self) -> Option<ShutdownReport> {
        let handle = lock(&self.handle).take()?;
        handle.interrupt_shutdown();
        Some(handle.join())
    }

    /// Block until the worker has taken every queued command.
    ///
    /// The shutdown interrupt is checked at the **top** of the worker's pass,
    /// before it reads any command, so a test that sends and interrupts in the
    /// same breath is asking about events that were never produced. Commands
    /// are dispatched in the same pass they are received, so an empty channel
    /// means the work is done — what is still open, deliberately, is whether
    /// the events it produced have been flushed yet.
    pub fn await_commands_taken(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        while self.pending_commands() > 0 {
            if Instant::now() >= deadline {
                panic!("the worker never took the queued commands");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // ---------------------------------------------------------------- clock

    /// Held under the device lock, which the driver also takes before reading
    /// the mode: without it a step already in flight advances the clock after
    /// the test believes it froze, and a preserved position drifts by a period.
    fn set_mode(&self, mode: u8) {
        let _device = lock(&self.device);
        self.driver.mode.store(mode, Ordering::Relaxed);
    }

    /// Play until the reported position reaches `target`, and stop there.
    ///
    /// The clock is stepped from this thread rather than left to the driver,
    /// and it stops whenever the worker stops accounting for the steps already
    /// taken. Neither half is optional. A free-running driver advances virtual
    /// time on its own schedule, so how far past `target` playback runs is a
    /// function of how long the scheduler kept the test thread away from the
    /// position it was watching — under CPU contention that overshot by the
    /// whole fixture. And a clock this thread steps as fast as it likes is not
    /// a device either: it outruns the worker, fills the 64-record span ring,
    /// and leaves the callback holding a span it could not publish, which is a
    /// state a later device loss is read through instead of the timeline.
    /// Waiting for the position to move keeps the clock inside what the engine
    /// has accounted for, so what a test measures here is the pipeline rather
    /// than the scheduler.
    pub fn play_for(&mut self, target: Duration) {
        // Behind the same barrier the closing `settle` is, and for a sharper
        // reason: the entry position decides whether this call plays at all.
        // `raw_position` on its own is the snapshot published before whatever
        // command the test sent last was applied, and a caller's `send` gives
        // that command a wall-clock budget rather than waiting for it. A
        // `Restart` still queued therefore reads as the pre-restart position,
        // `target` is already behind it, the loop breaks having played
        // nothing - and the restart then lands and rewinds to zero, so the
        // test measures a position taken from before the command it sent.
        self.settle();
        let deadline = Instant::now() + PATIENCE;
        let clock_at_entry = self.clock();
        let position_at_entry = self.raw_position();
        loop {
            let position = self.raw_position();
            if position >= target {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "position never reached {target:?}; it stalled at {position:?}; {}",
                    self.history()
                );
            }
            // How far the clock has run beyond the playback it is supposed to
            // be driving. One output latency of it is the pipeline and cannot
            // be helped: a span is published a latency before it is heard.
            // Anything past that is the worker not having caught up, and is
            // where the clock waits.
            let lead = self
                .clock()
                .saturating_sub(clock_at_entry)
                .saturating_sub(position.saturating_sub(position_at_entry));
            if lead < LATENCY + CLOCK_LEAD_SLACK {
                lock(&self.device).output.advance(PERIOD);
            }
            self.pump_events();
            std::thread::sleep(PACING_NAP);
        }
        self.settle();
    }

    pub fn play_to_end(&mut self) {
        self.set_mode(ADVANCING);
        let deadline = Instant::now() + PATIENCE;
        while !self.take_state(PlaybackState::Ended) {
            if Instant::now() >= deadline {
                self.set_mode(FROZEN);
                panic!("the track never ended");
            }
            self.pump_events();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.set_mode(FROZEN);
        self.settle();
    }

    /// Advance one period at a time until the callback has been handed nothing
    /// but silence twice running, which is what an empty ring sounds like, and
    /// stop there. The output latency means the final span's predicted play
    /// time is still far ahead, so end of track must not have fired yet.
    pub fn drain_ring_without_advancing_clock(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        let mut silent = 0;
        while silent < 2 {
            if Instant::now() >= deadline {
                panic!("the ring never drained");
            }
            {
                let mut device = lock(&self.device);
                device.output.clear_captured();
                device.output.advance(PERIOD);
                if device.output.captured().iter().all(|sample| *sample == 0.0) {
                    silent += 1;
                } else {
                    silent = 0;
                }
            }
            // Paced so the worker is never the reason the ring runs dry. Once a
            // period has come back silent the pause gets much longer, because
            // from here on the question is no longer how fast the ring drains
            // but whether it is really empty: a worker that a loaded machine
            // descheduled would refill it given the chance, and the point of
            // waiting is to give it that chance before calling the ring dry.
            let pause = if silent > 0 {
                SILENCE_GRACE
            } else {
                PACING_NAP
            };
            std::thread::sleep(pause);
        }
        self.pump_events();
    }

    /// Let virtual time pass without asking anything of the engine. Used to
    /// show that a parked transport does not move the position.
    pub fn let_time_pass(&mut self, span: Duration) {
        self.advance_clock(span);
        self.settle();
    }

    /// Like `let_time_pass`, but does not settle behind a command round trip
    /// afterward. `settle` needs the worker to take and answer a `SetVolume`,
    /// which a worker legitimately blocked inside a live network read
    /// (starvation, with nothing released to unblock it) cannot do — using
    /// `let_time_pass` there would wait out `settle`'s own patience rather
    /// than observe anything about the starved position. The position read
    /// afterward is the raw published value the worker's last completed pass
    /// left behind, which is exactly what "starvation does not advance it"
    /// is a claim about.
    pub fn let_time_pass_while_unresponsive(&mut self, span: Duration) {
        self.advance_clock(span);
    }

    pub fn advance_past_output_latency(&mut self) {
        self.advance_clock(LATENCY + LATENCY);
        self.pump_events();
    }

    /// Advance the virtual clock by `span`, a period at a time, and do not
    /// return until all of it has passed.
    ///
    /// The amount is what the caller asked for rather than whatever fitted in a
    /// real-time budget: a truncated advance is indistinguishable from an
    /// engine that failed to move, and on a loaded machine it is the budget
    /// that runs out first. The real-time bound is only a deadlock guard, and
    /// reaching it is a failure rather than an early exit.
    fn advance_clock(&mut self, span: Duration) {
        let deadline = Instant::now() + PATIENCE;
        let mut advanced = Duration::ZERO;
        while advanced < span {
            if Instant::now() >= deadline {
                panic!("only {advanced:?} of the requested {span:?} of virtual time passed");
            }
            lock(&self.device).output.advance(PERIOD);
            advanced += PERIOD;
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    // --------------------------------------------------------------- events

    fn pump_events(&self) {
        if !self.draining.load(Ordering::Relaxed) {
            return;
        }
        let handle = lock(&self.handle);
        let Some(handle) = handle.as_ref() else {
            return;
        };
        while let Ok(event) = handle.events().try_recv() {
            if let PlaybackEvent::StateChanged { state, .. } = &event {
                lock(&self.states).push(*state);
            }
            if matches!(event, PlaybackEvent::EndOfTrack { .. }) {
                self.saw_end_of_track.store(true, Ordering::Relaxed);
            }
            lock(&self.inbox).push(event);
        }
    }

    /// Whether an `EndOfTrack` was ever observed in this run.
    pub fn saw_end_of_track(&self) -> bool {
        self.pump_events();
        self.saw_end_of_track.load(Ordering::Relaxed)
    }

    /// Run until `Ended`, `Failed` or `patience`, whichever comes first.
    /// Drives the clock (`play_to_end`'s own ADVANCING mode) rather than
    /// leaving it frozen: a frozen clock never lets the output consume what
    /// `prime_and_run` already staged, so the ring stays full, `pump_audio`
    /// never has to read another byte, and a truncated or corrupt tail is
    /// never discovered at all - the very thing this exists to drive toward.
    pub fn play_until_terminal(&mut self, patience: Duration) {
        self.set_mode(ADVANCING);
        let deadline = Instant::now() + patience;
        loop {
            self.pump_events();
            if self.take_state(PlaybackState::Ended) || self.take_state(PlaybackState::Failed) {
                self.set_mode(FROZEN);
                return;
            }
            if Instant::now() >= deadline {
                self.set_mode(FROZEN);
                panic!(
                    "playback never reached a terminal state within {patience:?}; it went \
                     through {:?}",
                    lock(&self.states)
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `await_event` with the device clock running, for an event that only
    /// arrives once buffered audio has been played out.
    ///
    /// A disconnect reaches the decoder only when it next reads, and with the
    /// clock frozen the ring stays full so it never does - the same reason
    /// `play_until_terminal` above drives the clock rather than leaving it
    /// frozen. Anything a *disconnect* causes is awaited through here;
    /// anything a *command* causes is awaited with plain `await_event`.
    ///
    /// Closes behind the same command round trip `play_for` does, and for the
    /// same reason. This mode does not pace the clock against the playback it
    /// drives: while the consumer keeps up, `pump_audio` never sees a full
    /// ring and so never returns, and the worker publishes no new position
    /// for as long as that lasts. An event emitted the moment it finally does
    /// return therefore arrives *before* the pass that accounts for
    /// everything played since - so a test reading the position straight off
    /// this event would read one from seconds of playback ago. The barrier
    /// makes the position this returns behind the event that caused it.
    pub fn play_until_event(
        &mut self,
        predicate: impl Fn(&PlaybackEvent) -> bool,
    ) -> PlaybackEvent {
        self.set_mode(ADVANCING);
        let event = self.await_event(predicate);
        self.set_mode(FROZEN);
        self.settle();
        event
    }

    pub fn stop_draining_events(&mut self) {
        self.draining.store(false, Ordering::Relaxed);
    }

    pub fn resume_draining_events(&mut self) {
        self.draining.store(true, Ordering::Relaxed);
        self.pump_events();
    }

    pub fn try_event(&mut self) -> Option<PlaybackEvent> {
        self.pump_events();
        let mut inbox = lock(&self.inbox);
        if inbox.is_empty() {
            None
        } else {
            Some(inbox.remove(0))
        }
    }

    pub fn await_event(&mut self, predicate: impl Fn(&PlaybackEvent) -> bool) -> PlaybackEvent {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump_events();
            {
                let mut inbox = lock(&self.inbox);
                if let Some(index) = inbox.iter().position(&predicate) {
                    return inbox.remove(index);
                }
            }
            if Instant::now() >= deadline {
                panic!("no matching event arrived; saw {:?}", lock(&self.inbox));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The next `Loaded` event's position and disposition, so a test can
    /// assert on them without repeating `PlaybackEvent::Loaded { .. }`'s
    /// destructuring at every call site.
    pub fn await_loaded(&mut self) -> Loaded {
        let event = self.await_event(|e| matches!(e, PlaybackEvent::Loaded { .. }));
        let PlaybackEvent::Loaded {
            position,
            disposition,
            capabilities,
            ..
        } = event
        else {
            unreachable!("await_event's predicate already matched Loaded")
        };
        Loaded {
            position,
            disposition,
            capabilities,
        }
    }

    /// The position an explicit restart's `RestartEstablished` reported. G1:
    /// the only event a policy can key an explicit restart on.
    pub fn await_restart_established(&mut self) -> Duration {
        let event = self.await_event(|e| matches!(e, PlaybackEvent::RestartEstablished { .. }));
        let PlaybackEvent::RestartEstablished { position, .. } = event else {
            unreachable!("await_event's predicate already matched RestartEstablished")
        };
        position
    }

    /// Wait for `SeekCompleted` and return where it landed, and its
    /// provenance. Takes its own patience rather than `PATIENCE`: a remote
    /// seek's refinement can legitimately take longer than a local one's.
    pub fn await_seek_completed(&mut self, patience: Duration) -> SeekLanding {
        let deadline = Instant::now() + patience;
        loop {
            self.pump_events();
            {
                let mut inbox = lock(&self.inbox);
                if let Some(index) = inbox
                    .iter()
                    .position(|e| matches!(e, PlaybackEvent::SeekCompleted { .. }))
                {
                    let PlaybackEvent::SeekCompleted {
                        actual, provenance, ..
                    } = inbox.remove(index)
                    else {
                        unreachable!("the position above already matched SeekCompleted")
                    };
                    return SeekLanding { actual, provenance };
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "no SeekCompleted arrived within {patience:?}; saw {:?}",
                    lock(&self.inbox)
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Wind the engine down explicitly, rather than leaving it to `Drop` at
    /// scope exit. Every `engine_remote` test ends with this before shutting
    /// its `TestServer` down, so the worker's teardown - which may still be
    /// touching the socket the server owns - completes before the socket
    /// does.
    pub fn finish(&mut self) {
        let _ = self.shutdown_report();
    }

    pub fn count_events(&mut self, predicate: impl Fn(&PlaybackEvent) -> bool) -> usize {
        // Give anything still in flight a moment to arrive.
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            self.pump_events();
            std::thread::sleep(Duration::from_millis(5));
        }
        lock(&self.inbox).iter().filter(|e| predicate(e)).count()
    }

    /// Consume the next occurrence of `state` from the history, if it has
    /// happened yet.
    fn take_state(&self, state: PlaybackState) -> bool {
        let states = lock(&self.states);
        let mut consumed = lock(&self.consumed_states);
        match states[*consumed..].iter().position(|s| *s == state) {
            Some(offset) => {
                *consumed += offset + 1;
                true
            }
            None => false,
        }
    }

    /// The states the engine went through and every failure it reported,
    /// for a panic that has to explain itself from a CI log alone.
    fn history(&self) -> String {
        let failures: Vec<String> = lock(&self.inbox)
            .iter()
            .filter_map(|event| match event {
                PlaybackEvent::Failed { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        format!(
            "it went through {:?}, failing with {failures:?}",
            lock(&self.states)
        )
    }

    pub fn await_state(&mut self, state: PlaybackState) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump_events();
            if self.take_state(state) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("the engine never reached {state:?}; {}", self.history());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn state(&mut self) -> PlaybackState {
        self.pump_events();
        lock(&self.states)
            .last()
            .copied()
            .unwrap_or(PlaybackState::Idle)
    }

    // ------------------------------------------------------------- progress

    pub fn progress(&mut self) -> Progress {
        match lock(&self.handle).as_ref() {
            Some(handle) => handle.progress(),
            None => panic!("the engine is gone"),
        }
    }

    fn raw_position(&mut self) -> Duration {
        self.progress().position
    }

    /// The virtual device's clock.
    fn clock(&self) -> Duration {
        Duration::from_nanos(lock(&self.device).output.now().0)
    }

    /// The published position, once the worker has had a chance to publish
    /// everything the frozen clock implies. Reading straight after a command
    /// would otherwise see the snapshot from before it was dispatched.
    pub fn position(&mut self) -> Duration {
        self.settle();
        self.raw_position()
    }

    /// Wait until the published position accounts for every span the callback
    /// has produced. Only meaningful with the clock frozen, which is the only
    /// time a test compares positions.
    ///
    /// A round trip through the worker, not "the number stopped changing for a
    /// while". The worker drains the span ring and republishes the position at
    /// the top of every pass of its loop and dispatches commands at the bottom,
    /// so an event answering a command sent from here proves a whole pass ran
    /// after the last span was published — and with the clock frozen and the
    /// callback idle, no further span can appear. Watching the number for
    /// stability instead mistakes a worker a loaded machine has merely
    /// descheduled for one that has finished, and the position read then is
    /// short by whatever the worker had not yet drained.
    fn settle(&mut self) {
        if !self.draining.load(Ordering::Relaxed) {
            // Nothing is reading the event channel, so no answer can arrive.
            return;
        }
        // Idempotent: full volume is what the engine starts at, and no test
        // changes the volume before comparing positions.
        self.send(PlaybackCommand::SetVolume(Volume::FULL));
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump_events();
            {
                let mut inbox = lock(&self.inbox);
                let answer = inbox
                    .iter()
                    .position(|e| matches!(e, PlaybackEvent::VolumeChanged { .. }));
                if let Some(index) = answer {
                    inbox.remove(index);
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!("the worker never answered the barrier the position is read behind");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn join_within(&self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            {
                let handle = lock(&self.handle);
                match handle.as_ref() {
                    Some(handle) if handle.is_finished() => return true,
                    None => return true,
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        // Shut the worker down first: it needs the device thread alive to
        // answer the handshakes its teardown performs.
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            handle.interrupt_shutdown();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            handle.join();
        }
        self.driver.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// Load `name` against a device that negotiates `channels` output channels, and
/// return the message of the failure it produces.
///
/// Deliberately not a `TestEngine`: the engine never reaches `Paused` here, so
/// there is no transport to drive and nothing for the driver thread to do.
pub fn load_failure_on_device(name: &str, channels: u16) -> String {
    let device = Arc::new(Mutex::new(Device {
        output: TestOutput::new(channels, RATE, BUFFER_FRAMES, LATENCY),
        link: None,
    }));
    // Held for the call's duration, so the worker's fault receiver stays live.
    let (_faults, fault_rx) = crossbeam_channel::bounded(16);
    let handle =
        EngineHandle::spawn_with(Box::new(HarnessOutput::new(Arc::clone(&device))), fault_rx);
    let path = fixture(name);
    let sent = handle.commands().send(PlaybackCommand::Load {
        request: LoadRequestId::from_raw(1),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    });
    if sent.is_err() {
        panic!("the engine stopped accepting commands");
    }
    let deadline = Instant::now() + PATIENCE;
    loop {
        while let Ok(event) = handle.events().try_recv() {
            if let PlaybackEvent::Failed { message, .. } = event {
                return message;
            }
        }
        if Instant::now() >= deadline {
            panic!("the load never failed");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Load a path that cannot be opened, and report where the engine left the
/// position once it entered `Failed`.
///
/// A failed load must pin the position at the requested `start_at`, so a retry
/// resumes where the caller asked rather than at the beginning.
// `clippy.toml`'s test exemption covers `#[test]` fns, not bare helpers here.
#[allow(clippy::expect_used)]
pub fn failed_load_position(
    missing: &std::path::Path,
    start_at: Duration,
) -> (PlaybackState, Duration) {
    let device = Arc::new(Mutex::new(Device {
        output: TestOutput::new(CHANNELS, RATE, BUFFER_FRAMES, LATENCY),
        link: None,
    }));
    let (_faults, fault_rx) = crossbeam_channel::bounded(16);
    let handle =
        EngineHandle::spawn_with(Box::new(HarnessOutput::new(Arc::clone(&device))), fault_rx);
    let id = MediaId::LocalFile(
        tenuto::media::id::AbsolutePath::new(missing.to_path_buf()).expect("absolute path"),
    );
    handle
        .commands()
        .send(PlaybackCommand::Load {
            request: LoadRequestId::from_raw(1),
            media: id,
            source: SourceLocation::LocalPath(missing.to_path_buf()),
            resume: ResumeIntent::StartAt(start_at),
        })
        .expect("engine accepts the load");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Ok(PlaybackEvent::StateChanged {
            state: PlaybackState::Failed,
            ..
        }) = handle.events().recv_timeout(Duration::from_millis(50))
        {
            break;
        }
        if Instant::now() > deadline {
            panic!("the engine never reported a failed load");
        }
    }
    let progress = handle.progress();
    let state = PlaybackState::Failed;
    handle.interrupt_shutdown();
    handle.join();
    (state, progress.position)
}

/// A session whose device refuses to open: `Loaded` goes out, then negotiation
/// rejects the channel count and the load fails. Hands back what `app::run`
/// would have — every event, in order, and the final `Progress`.
///
/// Deliberately not a `TestEngine`: the engine never reaches `Paused` here, so
/// there is no transport to drive and nothing for a driver thread to do.
pub fn failed_device_session(name: &str, channels: u16, start_at: Duration) -> ShutdownReport {
    let device = Arc::new(Mutex::new(Device {
        output: TestOutput::new(channels, RATE, BUFFER_FRAMES, LATENCY),
        link: None,
    }));
    // Held for the call's duration, so the worker's fault receiver stays live.
    let (_faults, fault_rx) = crossbeam_channel::bounded(16);
    let handle =
        EngineHandle::spawn_with(Box::new(HarnessOutput::new(Arc::clone(&device))), fault_rx);
    let path = fixture(name);
    let sent = handle.commands().send(PlaybackCommand::Load {
        request: LoadRequestId::from_raw(1),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(start_at),
    });
    if sent.is_err() {
        panic!("the engine stopped accepting commands");
    }

    let mut events = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    loop {
        while let Ok(event) = handle.events().try_recv() {
            events.push(event);
        }
        if events
            .iter()
            .any(|event| matches!(event, PlaybackEvent::Failed { .. }))
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("the load never failed; saw {events:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    handle.interrupt_shutdown();
    let mut report = handle.join();
    events.extend(std::mem::take(&mut report.events));
    report.events = events;
    report
}

/// Like `failed_device_session`, but also sends `PlayLoaded` for the same
/// token, followed by a `SetVolume` barrier, to prove that a later
/// `PlayLoaded` does not reopen a device that already failed (M5 §6): the
/// `adopted_load == Some(request) && state == Paused` gate in `dispatch`
/// never sees `Paused` here, since the load's own synchronous open already
/// failed. Hands back every event up to the volume barrier, plus however
/// many times the device was asked to negotiate - exactly once is the direct
/// evidence "no second open attempt" needs, since a silent reopen that fails
/// the same way would be invisible in the event stream alone.
pub fn failed_device_session_with_play_loaded(
    name: &str,
    channels: u16,
) -> (Vec<PlaybackEvent>, usize) {
    let device = Arc::new(Mutex::new(Device {
        output: TestOutput::new(channels, RATE, BUFFER_FRAMES, LATENCY),
        link: None,
    }));
    let harness = HarnessOutput::new(Arc::clone(&device));
    let negotiations = Arc::clone(&harness.negotiations);
    // Held for the call's duration, so the worker's fault receiver stays live.
    let (_faults, fault_rx) = crossbeam_channel::bounded(16);
    let handle = EngineHandle::spawn_with(Box::new(harness), fault_rx);
    let path = fixture(name);
    let request = LoadRequestId::from_raw(1);
    if handle
        .commands()
        .send(PlaybackCommand::Load {
            request,
            media: MediaId::LocalFile(path.clone()),
            source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
            resume: ResumeIntent::StartAt(Duration::ZERO),
        })
        .is_err()
    {
        panic!("the engine stopped accepting commands");
    }
    if handle
        .commands()
        .send(PlaybackCommand::PlayLoaded { request })
        .is_err()
    {
        panic!("the engine stopped accepting commands");
    }
    if handle
        .commands()
        .send(PlaybackCommand::SetVolume(Volume::new(0.25)))
        .is_err()
    {
        panic!("the engine stopped accepting commands");
    }

    let mut events = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    loop {
        while let Ok(event) = handle.events().try_recv() {
            events.push(event);
        }
        if events
            .iter()
            .any(|event| matches!(event, PlaybackEvent::VolumeChanged { .. }))
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("the volume barrier never arrived; saw {events:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    handle.interrupt_shutdown();
    let mut report = handle.join();
    events.extend(std::mem::take(&mut report.events));

    (events, negotiations.load(Ordering::Relaxed))
}
