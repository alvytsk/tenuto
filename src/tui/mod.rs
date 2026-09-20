//! `tenuto tui`: startup, the event loop and teardown around
//! [`PlayerRuntime`] (design doc M5 §4, §11).
//!
//! Startup runs in the order §11 fixes: cleanup state and the panic hook,
//! signals, the profile lock, state, the session log and fd-2 redirect, the
//! writer and runtime, then the terminal. A shutdown request or a worker's
//! fatal panic recorded between two stages skips the rest, and every way out
//! — quit key, signal, worker fatal, startup failure, or a panic on this
//! thread — takes the same teardown over whatever was initialized so far.
//!
//! Each frame is drawn by [`render::draw`] in the size tier the terminal
//! allows (§7).
//!
//! The on-demand browser (§8) is owned here too: its state exists only while
//! it is open, its keys go to [`browser::BrowserState::handle_key`], and its
//! reads run on a [`BrowseWorker`] started the first time it opens.
//!
//! Cover art (§9) is detected once after entering the alternate screen,
//! loaded for the active local entry on an [`ArtworkWorker`] started the
//! first time one needs it, and prepared by [`images::CoverCache`] before
//! each draw — never inside it.
//!
//! The spectrum row (§10) enables the engine's analysis worker only while
//! the row exists and playback is `Playing`, and its levels are chosen and
//! decayed by [`spectrum::SpectrumDisplay`] before each draw, the same way.

pub mod browser;
pub mod images;
pub mod input;
pub mod layout;
pub mod render;
pub mod spectrum;
pub mod state;
pub mod tabs;
pub mod theme;

use std::any::Any;
use std::io::{self, Stdout, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::Hide;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::application::browse::{BrowseRequest, BrowseResult, BrowseWorker, TreeCollected};
use crate::application::enrich::default_probe;
use crate::application::runtime::{
    AppCommand, CoverKey, FlushReport, LibraryStores, PlayerRuntime, RuntimeParts,
};
use crate::application::view::PlayerView;
use crate::artwork::worker::{ArtworkWorker, CoverSource, default_loader};
use crate::cli::{ArtworkMode, MouseMode};
use crate::clock::{Clock, SystemClock};
use crate::error::{AppError, LifecycleError};
use crate::http::limits::Limits;
use crate::lifecycle::RunOutcome;
use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::input::InputReader;
use crate::lifecycle::lock::{LockError, ProfileLock};
use crate::lifecycle::panic::{FatalCleanup, install_panic_hook};
use crate::lifecycle::signals::ShutdownSignals;
use crate::lifecycle::terminal::KITTY_DELETE_ALL;
use crate::media::id::MediaId;
use crate::persistence::store::{LoadOutcome, QueueBackup, StateStore};
use crate::persistence::writer::{DisabledSink, StateSink, WriterHandle};
use crate::playback::engine::EngineHandle;
use crate::session::Session;
use crate::tui::browser::{BrowserEffect, BrowserState};
use crate::tui::images::{CoverCache, failure_status, picker_for, query_terminal};
use crate::tui::input::{Effect, handle_key, handle_mouse, routes_to_browser};
use crate::tui::layout::{regions, tier_for};
use crate::tui::render::{CoverView, HitMap, Visuals};
use crate::tui::spectrum::{DrawSource, SpectrumDisplay, wants_analysis};
use crate::tui::state::{Overlay, UiState};

/// How long one loop pass waits for terminal input before pumping the
/// runtime again; also the bound on how late a signal or fatal panic is seen.
const INPUT_POLL: Duration = Duration::from_millis(50);
const STATE_NOT_SAVED: &str = "This session is not saved";

pub struct TuiOptions {
    pub mouse: MouseMode,
    pub artwork: ArtworkMode,
}

type Tty = Terminal<CrosstermBackend<Stdout>>;

/// What startup has initialized, for teardown to release.
#[derive(Default)]
struct Stages {
    lock: Option<ProfileLock>,
    runtime: Option<PlayerRuntime>,
    terminal: Option<Tty>,
}

/// Why the run is ending.
enum Ending {
    /// The quit key, the Ctrl-C key, or an OS signal.
    Requested,
    /// A panic on another thread took the fatal path.
    WorkerPanicked,
    Failed(AppError),
    /// A panic on this thread, to be resumed once teardown is done.
    Panicked(Box<dyn Any + Send>),
}

pub fn run(options: TuiOptions) -> Result<RunOutcome, AppError> {
    let hook = TestHook::from_env();
    // The loop polls `fatal_requested` at least every `INPUT_POLL`, so the
    // wake receiver only has to stay alive for the hook's `try_send`.
    let (wake, _wake_receiver) = crossbeam_channel::bounded(1);
    let cleanup = Arc::new(FatalCleanup::new(wake));
    install_panic_hook(Arc::clone(&cleanup));

    let signals = ShutdownSignals::install().map_err(LifecycleError::Signals)?;

    let mut stages = Stages::default();
    let ending = match panic::catch_unwind(AssertUnwindSafe(|| {
        start_and_loop(hook, &options, &cleanup, &signals, &mut stages)
    })) {
        Ok(ending) => ending,
        Err(payload) => Ending::Panicked(payload),
    };
    teardown(ending, stages, &cleanup, signals)
}

/// `Some` when a shutdown request or a fatal panic elsewhere means startup
/// must not continue.
fn interrupted(signals: &ShutdownSignals, cleanup: &FatalCleanup) -> Option<Ending> {
    if cleanup.fatal_requested() {
        Some(Ending::WorkerPanicked)
    } else if signals.requested() {
        Some(Ending::Requested)
    } else {
        None
    }
}

macro_rules! stage {
    ($signals:expr, $cleanup:expr) => {
        if let Some(ending) = interrupted($signals, $cleanup) {
            return ending;
        }
    };
}

macro_rules! attempt {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => return Ending::Failed(AppError::from(error)),
        }
    };
}

fn start_and_loop(
    hook: TestHook,
    options: &TuiOptions,
    cleanup: &FatalCleanup,
    signals: &ShutdownSignals,
    stages: &mut Stages,
) -> Ending {
    stage!(signals, cleanup);

    hook.panic_at(TestHook::PanicBeforeRedirect);
    let state_path = attempt!(
        StateStore::platform_path().map_err(|_| LifecycleError::from(LockError::NoStateDirectory))
    );
    stages.lock = Some(attempt!(
        ProfileLock::acquire(&state_path).map_err(LifecycleError::from)
    ));
    stage!(signals, cleanup);

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let store = StateStore::new(state_path.clone(), Arc::clone(&clock));
    let loaded = store.load();
    stage!(signals, cleanup);

    attempt!(redirect_to_session_log(&state_path, cleanup));
    hook.panic_at(TestHook::PanicAfterRedirect);
    stage!(signals, cleanup);

    let runtime = stages
        .runtime
        .insert(start_runtime(store, loaded, clock, hook));
    stage!(signals, cleanup);

    let terminal = stages.terminal.insert(attempt!(
        enter_terminal(options.mouse, cleanup).map_err(LifecycleError::Terminal)
    ));
    hook.panic_at(TestHook::PanicAfterTerminal);
    if hook == TestHook::StderrProbe {
        probe_stderr();
    }
    stage!(signals, cleanup);

    // Once, in the alternate screen and before the loop reads any input:
    // the query's answer arrives on stdin.
    let picker = picker_for(options.artwork, query_terminal);
    if picker
        .as_ref()
        .is_some_and(|picker| picker.protocol_type() == ProtocolType::Kitty)
    {
        cleanup.terminal().set_kitty_images(true);
    }
    let artwork = Artwork::new(options.artwork, picker, hook);
    stage!(signals, cleanup);

    let ui = UiState::new(options.mouse == MouseMode::On);
    run_loop(runtime, terminal, ui, artwork, cleanup, signals)
}

/// Opens this run's log, hands a clone to the panic hook for contained-panic
/// diagnostics, and points fd 2 at it. The redirect goes straight into the
/// cleanup slot with nothing fallible in between, so a panic at any later
/// point restores fd 2.
#[cfg(unix)]
fn redirect_to_session_log(
    state_path: &std::path::Path,
    cleanup: &FatalCleanup,
) -> Result<(), LifecycleError> {
    use crate::lifecycle::stderr::{open_session_log, redirect_stderr};

    let (file, _path) = open_session_log(state_path, time::OffsetDateTime::now_utc())
        .map_err(LifecycleError::Log)?;
    cleanup.set_diagnostic_log(file.try_clone().map_err(LifecycleError::Log)?);
    let redirect = redirect_stderr(file).map_err(LifecycleError::Redirect)?;
    // The slot starts empty and nothing else publishes into it, so this
    // cannot be refused; a refused value would be dropped here, restoring
    // fd 2 at once rather than leaking the redirect.
    let _ = cleanup.stderr_slot().publish(Box::new(redirect));
    Ok(())
}

/// Without fd-2 redirection, C libraries and stray diagnostics would write
/// over the interface, so the terminal player refuses to start rather than
/// run without it.
#[cfg(not(unix))]
fn redirect_to_session_log(
    _state_path: &std::path::Path,
    _cleanup: &FatalCleanup,
) -> Result<(), LifecycleError> {
    Err(LifecycleError::Redirect(io::Error::new(
        io::ErrorKind::Unsupported,
        "the terminal player needs stderr redirection, which is available only on Unix",
    )))
}

fn start_runtime(
    store: StateStore,
    loaded: LoadOutcome,
    clock: Arc<dyn Clock>,
    hook: TestHook,
) -> PlayerRuntime {
    let LoadOutcome {
        state,
        writable,
        queue_repair,
        ..
    } = loaded;
    let sink: Box<dyn StateSink> = if writable {
        Box::new(store)
    } else {
        Box::new(DisabledSink)
    };
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(&clock)),
        persisting: writable,
        clock,
        engine_factory: Box::new(EngineHandle::spawn_for_environment),
        library: library_stores(),
        http_limits: Limits::default(),
        metadata_probe: Some(default_probe(hook)),
        hook,
    });
    let status = match queue_repair {
        Some(repair) => Some(match repair.backup {
            QueueBackup::Saved(path) => format!(
                "Queue data was reset ({}); backup at {}",
                repair.reset.fields_reset(),
                path.display()
            ),
            QueueBackup::Failed => format!(
                "Queue data was reset ({}); this session is not saved",
                repair.reset.fields_reset()
            ),
        }),
        None if !writable => Some(STATE_NOT_SAVED.to_owned()),
        None => None,
    };
    if let Some(status) = status {
        runtime.set_status(status);
    }
    runtime
}

/// The platform's subscription, feed-cache and station stores, or `None`
/// when there is no platform data directory. Both the runtime and the
/// browse worker call this, each owning its own stores.
fn library_stores() -> Option<LibraryStores> {
    let (subscriptions, cache) = crate::commands::platform_subscription_stores().ok()?;
    let stations = crate::commands::platform_station_store().ok()?;
    Some(LibraryStores {
        subscriptions,
        cache,
        stations,
    })
}

/// Each change is marked for cleanup as soon as it is made, so a failure
/// part-way leaves teardown exactly what needs undoing.
fn enter_terminal(mouse: MouseMode, cleanup: &FatalCleanup) -> io::Result<Tty> {
    let terminal = cleanup.terminal();
    enable_raw_mode()?;
    terminal.mark_raw();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    terminal.mark_alternate();
    execute!(stdout, Hide)?;
    terminal.mark_cursor_hidden();
    if mouse == MouseMode::On {
        execute!(stdout, EnableMouseCapture)?;
        terminal.set_mouse(true);
    }
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// A write to fd 2 from a child process and one from Rust, so a process test
/// can check both reach the session log instead of the terminal.
fn probe_stderr() {
    let _ = std::process::Command::new("sh")
        .args(["-c", "printf tenuto-stderr-probe >&2"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status();
    eprintln!("tenuto-stderr-probe-rust");
}

fn run_loop(
    runtime: &mut PlayerRuntime,
    terminal: &mut Tty,
    mut ui: UiState,
    mut artwork: Artwork,
    cleanup: &FatalCleanup,
    signals: &ShutdownSignals,
) -> Ending {
    // Where the last frame put its clickable parts, for mouse input (Task 21).
    let mut hits = HitMap::default();
    let mut browsing = Browsing::default();
    let mut spectrum = SpectrumDisplay::default();
    // Only now: the cover-art query in `start_and_loop` has read its answer.
    let input = match InputReader::spawn(MAX_EVENTS_PER_PASS) {
        Ok(input) => input,
        Err(error) => return Ending::Failed(LifecycleError::Terminal(error).into()),
    };
    loop {
        let mut front = Front {
            runtime,
            ui: &mut ui,
            browsing: &mut browsing,
            artwork: &mut artwork,
            cleanup,
            signals,
        };
        if let Err(error) = handle_input(&mut front, &hits, &input) {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
        runtime.pump();
        // A finished folder walk belongs to the application, not the
        // browser that asked for it (M8 §8): it lands here even if the
        // browser has since closed or moved to another directory.
        let trees = browsing.poll();
        let landed_a_tree = !trees.is_empty();
        for tree in trees {
            runtime.handle(AppCommand::AddTree(tree));
        }
        if landed_a_tree {
            let hint = runtime.take_selection_hint();
            ui.reconcile(&runtime.view(), hint);
        }
        // Every pass, so the worker's one-slot result channel never stalls it.
        artwork.poll(runtime);
        if let Some(ending) = interrupted(signals, cleanup) {
            return ending;
        }
        if cleanup.rendering_disabled() {
            continue;
        }
        // Before the view, so an encoding failure's status shows this frame.
        if let Err(error) = artwork.prepare(runtime, terminal, &ui, cleanup) {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
        let view = runtime.view();
        ui.reconcile(&view, None);
        if let Some(browser) = &mut browsing.state {
            browser.sync_queue(&view.rows);
        }
        if let Err(error) = update_spectrum(runtime, terminal, &view, &mut spectrum) {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
        let visuals = Visuals {
            cover: artwork
                .covers
                .widget()
                .map_or(CoverView::Placeholder, CoverView::Image),
            browser: browsing.state.as_ref(),
            spectrum: spectrum.levels(),
            peaks: spectrum.peaks(),
        };
        // Checked again at the last moment: a worker's fatal panic can
        // restore the primary screen while this pass prepares its frame, and
        // a draw after that would paint over the panic diagnostic.
        if cleanup.rendering_disabled() {
            continue;
        }
        if let Err(error) = terminal.draw(|frame| {
            hits = render::draw(frame, &view, &ui, &visuals);
        }) {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
    }
}

/// Runs analysis only while this frame has a spectrum row and playback is
/// `Playing`, then settles the levels the row will draw: the latest frame's
/// when it is fresh and belongs to the adopted playback, otherwise the
/// previous levels decayed.
fn update_spectrum(
    runtime: &PlayerRuntime,
    terminal: &Tty,
    view: &PlayerView,
    display: &mut SpectrumDisplay,
) -> io::Result<()> {
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let row = regions(
        area,
        tier_for(area.width, area.height),
        render::info_line_count(view),
    )
    .spectrum;
    let frame = runtime.spectrum().and_then(|handle| {
        handle.set_enabled(wants_analysis(row, view.phase));
        handle.latest()
    });
    display.update(
        &DrawSource {
            phase: view.phase,
            now_playing: view.now_playing.as_ref(),
            adopted: runtime.session().adopted().map(|load| load.request),
            frame: frame.as_ref(),
        },
        Instant::now(),
    );
    Ok(())
}

/// Cover art for the active entry: the detected picker (none under
/// `--artwork off`), the worker that loads covers, and the prepared cover.
struct Artwork {
    mode: ArtworkMode,
    picker: Option<Picker>,
    hook: TestHook,
    /// Started the first time a local entry becomes active, never under
    /// `--artwork off`.
    worker: Option<ArtworkWorker>,
    /// The entry whose cover was last requested, **and the source it was
    /// requested from**; `None` while the active entry has no cover source
    /// (see `PlayerRuntime::active_cover`). The source belongs in the key
    /// because a re-probe can change a station's (or a feed's) artwork URL
    /// without changing its media identity (M7.1 §8.1).
    requested: Option<(MediaId, CoverSource)>,
    /// The `cover_key` last seen; the cover is resolved only when it moves,
    /// because resolving a podcast's cover reads the feed cache from disk.
    key: Option<CoverKey>,
    covers: CoverCache,
}

impl Artwork {
    fn new(mode: ArtworkMode, picker: Option<Picker>, hook: TestHook) -> Self {
        Self {
            mode,
            picker,
            hook,
            worker: None,
            requested: None,
            key: None,
            covers: CoverCache::new(hook),
        }
    }

    /// Requests the cover when the active entry's cover source changes, shows
    /// the placeholder meanwhile and for every other entry, and installs a
    /// finished cover if it is still for the active entry. A failed load
    /// leaves the placeholder and, unless there simply is no artwork, a
    /// status message; a result for an entry no longer active is dropped
    /// silently.
    fn poll(&mut self, runtime: &mut PlayerRuntime) {
        if self.mode == ArtworkMode::Off {
            return;
        }
        let key = runtime.cover_key();
        if key != self.key {
            self.key = key;
            match runtime.active_cover() {
                // A key move for a media already requested from the same
                // source (its load landed, say) resolves to the same cover;
                // only a new entry, or one whose cover just became
                // available or changed source (a re-probe, §8.1), is
                // requested.
                Some((media, source))
                    if self.requested.as_ref() == Some(&(media.clone(), source.clone())) => {}
                Some((media, source)) => {
                    self.covers.set_image(media.clone(), None);
                    let hook = self.hook;
                    self.worker
                        .get_or_insert_with(|| ArtworkWorker::spawn(default_loader(hook)))
                        .request(media.clone(), source.clone());
                    self.requested = Some((media, source));
                }
                None => {
                    if let Some((previous, _)) = self.requested.take() {
                        self.covers.set_image(previous, None);
                    }
                }
            }
        }
        let Some(worker) = &self.worker else {
            return;
        };
        while let Some(result) = worker.try_result() {
            if self.requested.as_ref().map(|(media, _)| media) != Some(&result.media) {
                continue;
            }
            let image = match result.image {
                Ok(image) => Some(image),
                Err(error) => {
                    tracing::debug!(%error, "no cover art for the active entry");
                    if let Some(status) = failure_status(&error) {
                        runtime.set_status(status);
                    }
                    None
                }
            };
            self.covers.set_image(result.media, image);
        }
    }

    /// Prepares the cover for this frame's cover region, then clears the
    /// terminal if a placement from an earlier frame may be stale.
    ///
    /// Sixel and iTerm2 images are drawn over the cells rather than in them,
    /// so an overlay could not hide one: while an overlay is open those
    /// protocols show the placeholder instead. A failed encoding sets a
    /// status message, once per failed key.
    fn prepare(
        &mut self,
        runtime: &mut PlayerRuntime,
        terminal: &mut Tty,
        ui: &UiState,
        cleanup: &FatalCleanup,
    ) -> io::Result<()> {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let covered = ui.overlay != Overlay::None
            && self.picker.as_ref().is_some_and(|picker| {
                matches!(
                    picker.protocol_type(),
                    ProtocolType::Sixel | ProtocolType::Iterm2
                )
            });
        // The cover is the same however many information lines there are.
        let cover = regions(area, tier_for(area.width, area.height), 1)
            .cover
            .filter(|_| !covered);
        self.covers.prepare(self.picker.as_ref(), self.mode, cover);
        if let Some(status) = self.covers.take_failure().as_ref().and_then(failure_status) {
            runtime.set_status(status);
        }
        // Never once fatal cleanup has begun: the clear would land on the
        // restored primary screen.
        if !cleanup.rendering_disabled() && self.covers.take_placement_cleanup() {
            if self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.protocol_type() == ProtocolType::Kitty)
            {
                let mut stdout = io::stdout();
                stdout.write_all(KITTY_DELETE_ALL)?;
                stdout.flush()?;
            }
            clear_screen(terminal, area)?;
        }
        Ok(())
    }
}

/// What `terminal.clear()` does for a full-screen terminal — erase the
/// screen and forget the last frame, so the next draw repaints every cell —
/// without its cursor-position query. That query blocks for up to two
/// seconds and then fails when the terminal does not answer, which would
/// turn a recovery redraw into a fatal terminal error.
fn clear_screen(terminal: &mut Tty, area: Rect) -> io::Result<()> {
    terminal.resize(area)
}

/// The browser while it is open, and the worker that reads for it.
#[derive(Default)]
struct Browsing {
    state: Option<BrowserState>,
    /// Started the first time the browser opens, then kept for the run.
    worker: Option<BrowseWorker>,
}

impl Browsing {
    /// Opens the browser at the active local entry's directory, else the
    /// current working directory, and asks for its listing. Captures the
    /// viewed playlist as the destination every add from this browser lands
    /// in, for as long as it stays open (M8 §8).
    fn open(&mut self, runtime: &PlayerRuntime, ui: &mut UiState) {
        let cwd = runtime
            .active_local_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.state = Some(BrowserState::new(cwd.clone(), runtime.viewed()));
        ui.overlay = Overlay::Browser;
        self.request(BrowseRequest::Directory(cwd));
    }

    fn close(&mut self, ui: &mut UiState) {
        self.state = None;
        if ui.overlay == Overlay::Browser {
            ui.overlay = Overlay::None;
        }
    }

    fn request(&mut self, request: BrowseRequest) {
        self.worker
            .get_or_insert_with(|| BrowseWorker::spawn(library_stores()))
            .request(request);
    }

    /// Hands every finished read to the open browser and returns the folder
    /// walks, which belong to the application: an add the listener asked for
    /// must land even if the browser has since closed or moved (M8 §8).
    fn poll(&mut self) -> Vec<TreeCollected> {
        let Some(worker) = &self.worker else {
            return Vec::new();
        };
        let mut trees = Vec::new();
        while let Some(result) = worker.try_result() {
            match result {
                BrowseResult::TreeCollected(tree) => trees.push(tree),
                result => {
                    if let Some(state) = &mut self.state
                        && let Some(follow_up) = state.apply(result)
                    {
                        worker.request(follow_up);
                    }
                }
            }
        }
        trees
    }
}

/// Everything an input event can act on.
struct Front<'a> {
    runtime: &'a mut PlayerRuntime,
    ui: &'a mut UiState,
    browsing: &'a mut Browsing,
    artwork: &'a mut Artwork,
    cleanup: &'a FatalCleanup,
    signals: &'a ShutdownSignals,
}

/// Handles pending input: mouse motion already queued is drained, up to
/// [`MAX_EVENTS_PER_PASS`], so a flood of it cannot hold a key back by one
/// frame per motion event; the pass ends after the first event that can act
/// (see [`drain_after`]) or once a quit request or fatal panic is recorded.
fn handle_input(front: &mut Front<'_>, hits: &HitMap, input: &InputReader) -> io::Result<()> {
    drain_events(
        |wait| input.next(wait),
        |event| {
            let next = drain_after(&event);
            handle_event(front, hits, event)?;
            Ok(if interrupted(front.signals, front.cleanup).is_some() {
                Drain::Stop
            } else {
                next
            })
        },
    )
    .map(|_| ())
}

/// Whether draining may go on after `event`. Only mouse motion, which no
/// handler acts on, lets it: every other event keeps a frame of its own, as
/// before, so a click is always tested against the frame drawn after the
/// previous action.
fn drain_after(event: &Event) -> Drain {
    match event {
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::Moved | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
            ) =>
        {
            Drain::Continue
        }
        _ => Drain::Stop,
    }
}

/// At most this many input events are handled before the loop pumps and
/// draws again, so an endless flood cannot starve drawing.
const MAX_EVENTS_PER_PASS: usize = 256;

/// Whether draining may go on to the next pending event this pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drain {
    Continue,
    Stop,
}

/// Waits up to [`INPUT_POLL`] for the first event, then takes only what is
/// already pending, handing each to `handle` until the queue is empty,
/// `handle` says stop, or [`MAX_EVENTS_PER_PASS`] events were handled.
/// Returns how many were handled. `next` waits up to the given time for an
/// event; it is generic so the bound and the stop rule can be tested without
/// a terminal.
fn drain_events(
    mut next: impl FnMut(Duration) -> io::Result<Option<Event>>,
    mut handle: impl FnMut(Event) -> io::Result<Drain>,
) -> io::Result<usize> {
    let mut wait = INPUT_POLL;
    let mut handled = 0;
    while handled < MAX_EVENTS_PER_PASS {
        let Some(event) = next(wait)? else {
            break;
        };
        wait = Duration::ZERO;
        handled += 1;
        if handle(event)? == Drain::Stop {
            break;
        }
    }
    Ok(handled)
}

/// Runs one input event through `tui::input::handle_key` or
/// `tui::input::handle_mouse` — whichever the event is — executing whatever
/// effects come back exactly the same way for either. While the browser is
/// open every key but Ctrl-C and Ctrl-L goes to the browser instead, and its
/// effects are executed here too. `hits` is where the last drawn frame put
/// its clickable parts (Task 19) — the frame the listener was looking at. A
/// resize invalidates the prepared cover, whose placement no longer matches
/// the screen; every other event kind (focus, paste) is ignored here since
/// the next loop pass redraws unconditionally.
fn handle_event(front: &mut Front<'_>, hits: &HitMap, event: Event) -> io::Result<()> {
    let view = front.runtime.view();
    let effects = match event {
        Event::Key(key) if routes_to_browser(&key, front.ui) => {
            let Some(browser) = &mut front.browsing.state else {
                // An overlay with no browser behind it has nothing to show.
                front.browsing.close(front.ui);
                return Ok(());
            };
            // The destination's rows, not the viewed playlist's: while the
            // two usually agree, the browser's ticks and Enter-to-remove
            // must always follow where its own adds land (M8 §8).
            browser.sync_queue(&front.runtime.rows_of(browser.dest));
            let effects = browser.handle_key(key);
            for effect in effects {
                apply_browser_effect(effect, front)?;
            }
            return Ok(());
        }
        Event::Key(key) => handle_key(key, front.ui, &view),
        Event::Mouse(mouse) => handle_mouse(mouse, hits, front.ui, &view),
        Event::Resize(..) => {
            front.artwork.covers.invalidate();
            Vec::new()
        }
        _ => Vec::new(),
    };
    for effect in effects {
        apply_effect(effect, front)?;
    }
    Ok(())
}

/// Executes one effect the browser returned: a read goes to the worker, an
/// enqueue is the ordinary enqueue command, and a close is `CloseBrowser`.
/// §8 does not say an enqueue closes the browser, so it stays open; the
/// browser has already cleared its marks.
fn apply_browser_effect(effect: BrowserEffect, front: &mut Front<'_>) -> io::Result<()> {
    match effect {
        BrowserEffect::Request(request) => {
            front.browsing.request(request);
            Ok(())
        }
        BrowserEffect::Enqueue { dest, items } => {
            apply_effect(Effect::App(AppCommand::Enqueue { dest, items }), front)
        }
        BrowserEffect::Remove(id) => apply_effect(Effect::App(AppCommand::Remove(id)), front),
        BrowserEffect::Close => apply_effect(Effect::CloseBrowser, front),
    }
}

/// Executes one effect `tui::input::handle_key` returned.
fn apply_effect(effect: Effect, front: &mut Front<'_>) -> io::Result<()> {
    match effect {
        Effect::App(command) => {
            front.runtime.handle(command);
            let hint = front.runtime.take_selection_hint();
            front.ui.reconcile(&front.runtime.view(), hint);
        }
        Effect::Notice(message) => front.runtime.set_status(message),
        Effect::Quit => front.signals.request(),
        // Not once fatal cleanup has begun, which has already released the
        // terminal: re-enabling capture would leave the primary screen
        // reporting mouse events to a shell.
        Effect::SetMouseCapture(_) if front.cleanup.rendering_disabled() => {}
        Effect::SetMouseCapture(on) => {
            let mut stdout = io::stdout();
            if on {
                execute!(stdout, EnableMouseCapture)?;
            } else {
                execute!(stdout, DisableMouseCapture)?;
            }
            front.cleanup.terminal().set_mouse(on);
        }
        // Invalidating requests placement cleanup, which the loop answers by
        // clearing the screen before the next draw; the cover is then
        // prepared again.
        Effect::FullRedraw => front.artwork.covers.invalidate(),
        Effect::OpenBrowser => front.browsing.open(front.runtime, front.ui),
        Effect::CloseBrowser => front.browsing.close(front.ui),
    }
    Ok(())
}

/// Releases what startup initialized, in §11's order: the engine and writer
/// (while fd 2 still points at the log), then the terminal and fd 2, then
/// the signal listener, then the profile lock, and only then anything
/// printed for the user.
fn teardown(
    ending: Ending,
    stages: Stages,
    cleanup: &FatalCleanup,
    signals: ShutdownSignals,
) -> Result<RunOutcome, AppError> {
    let Stages {
        lock,
        runtime,
        terminal,
    } = stages;
    // Ratatui shows the cursor when its terminal drops; doing that now keeps
    // any complaint about a vanished PTY in the log.
    drop(terminal);
    let flush = runtime.map(PlayerRuntime::shutdown);
    cleanup.restore_now();
    let outcome = signals.outcome();
    signals.close();
    drop(lock);

    // `writeln!`, not `eprintln!`: the pane's PTY may be gone by now, and
    // `eprintln!` panics when the write fails.
    let mut stderr = io::stderr();
    match flush {
        Some(FlushReport::Failed(error)) => {
            let _ = writeln!(stderr, "State was not saved: {error}");
        }
        Some(FlushReport::Unconfirmed) => {
            let _ = writeln!(
                stderr,
                "State was not saved: the final write was not confirmed in time"
            );
        }
        Some(FlushReport::Written | FlushReport::Disabled) | None => {}
    }

    match ending {
        Ending::Panicked(payload) => panic::resume_unwind(payload),
        Ending::WorkerPanicked => Err(LifecycleError::WorkerPanicked.into()),
        // As in `play`, a recorded signal is what the run reports, even when
        // it arrived alongside a failure (a hung-up pane, say).
        Ending::Failed(error) => match outcome {
            RunOutcome::Signalled(_) => Ok(outcome),
            RunOutcome::Completed => Err(error),
        },
        Ending::Requested => Ok(outcome),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;

    fn motion() -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn key() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
    }

    /// Drains `pending` once, recording each poll's wait.
    fn pass(
        pending: &mut VecDeque<Event>,
        waits: &mut Vec<Duration>,
        mut handle: impl FnMut(Event) -> Drain,
    ) -> usize {
        drain_events(
            |wait| {
                waits.push(wait);
                Ok(pending.pop_front())
            },
            |event| Ok(handle(event)),
        )
        .unwrap_or_else(|error| panic!("drain: {error}"))
    }

    #[test]
    fn a_motion_flood_is_drained_in_bounded_passes_and_the_key_behind_it_is_reached() {
        let mut pending: VecDeque<Event> = std::iter::repeat_with(motion).take(600).collect();
        pending.push_back(key());
        let mut keys = 0;
        let mut waits = Vec::new();

        let first = pass(&mut pending, &mut waits, |event| {
            keys += usize::from(matches!(event, Event::Key(_)));
            drain_after(&event)
        });
        assert_eq!(
            first, MAX_EVENTS_PER_PASS,
            "bounded, so drawing is not starved"
        );
        assert_eq!(waits[0], INPUT_POLL, "only the first poll waits");
        assert!(waits[1..].iter().all(|wait| wait.is_zero()), "{waits:?}");

        let mut passes = 1;
        while keys == 0 {
            pass(&mut pending, &mut Vec::new(), |event| {
                keys += usize::from(matches!(event, Event::Key(_)));
                drain_after(&event)
            });
            passes += 1;
        }
        assert_eq!(passes, 3, "601 events take three passes, not 601");
        assert!(pending.is_empty());
    }

    #[test]
    fn only_mouse_motion_lets_draining_continue() {
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        for passing in [
            motion(),
            mouse(MouseEventKind::Drag(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
        ] {
            assert_eq!(drain_after(&passing), Drain::Continue, "{passing:?}");
        }
        for acting in [
            key(),
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::ScrollDown),
            Event::Resize(80, 24),
        ] {
            assert_eq!(drain_after(&acting), Drain::Stop, "{acting:?}");
        }
    }

    #[test]
    fn a_key_behind_buffered_keys_keeps_its_own_pass() {
        let mut pending: VecDeque<Event> = [key(), key()].into_iter().collect();
        assert_eq!(
            pass(&mut pending, &mut Vec::new(), |event| drain_after(&event)),
            1
        );
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn draining_stops_when_the_handler_says_so_and_waits_once_when_idle() {
        let mut pending: VecDeque<Event> = [key(), key(), key()].into_iter().collect();
        let handled = pass(&mut pending, &mut Vec::new(), |_| Drain::Stop);
        assert_eq!(handled, 1);
        assert_eq!(pending.len(), 2, "the rest wait for the next pass");

        let mut idle = VecDeque::new();
        let mut waits = Vec::new();
        assert_eq!(pass(&mut idle, &mut waits, |_| Drain::Continue), 0);
        assert_eq!(waits, vec![INPUT_POLL]);
    }

    // ---------------------------------------------------- Browsing::poll

    use crate::playlist::PlaylistId;

    fn one_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap_or_else(|error| panic!("write: {error}"));
        path
    }

    /// Polls `browsing` until it returns at least one tree, or panics past a
    /// generous deadline — the same bounded-wait shape `tests/m5_browser.rs`
    /// uses for `BrowseWorker::try_result`.
    fn poll_for_a_tree(browsing: &mut Browsing) -> Vec<TreeCollected> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let trees = browsing.poll();
            if !trees.is_empty() {
                return trees;
            }
            assert!(Instant::now() < deadline, "no tree ever arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_tree_lands_once_the_browser_that_asked_for_it_has_closed() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let file = one_file(dir.path(), "track.mp3");
        let dest = PlaylistId::from_raw_for_tests(1);

        let mut browsing = Browsing {
            state: Some(BrowserState::new(dir.path().to_path_buf(), dest)),
            worker: None,
        };
        browsing.request(BrowseRequest::CollectTree {
            roots: vec![dir.path().to_path_buf()],
            dest,
        });
        // Closed before the worker has necessarily answered: the request was
        // already in flight, and its answer must still reach the caller.
        browsing.close(&mut UiState::new(false));
        assert!(
            browsing.state.is_none(),
            "precondition: the browser is closed before the tree is drained"
        );

        let trees = poll_for_a_tree(&mut browsing);
        assert_eq!(trees.len(), 1, "{trees:?}");
        assert_eq!(trees[0].dest, dest);
        assert_eq!(trees[0].items, vec![file]);
        assert!(
            browsing.state.is_none(),
            "still closed once the tree has landed"
        );
    }

    #[test]
    fn a_tree_lands_in_its_captured_destination_though_the_browser_moved_elsewhere() {
        let asked_dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let file = one_file(asked_dir.path(), "track.flac");
        let dest = PlaylistId::from_raw_for_tests(7);
        let elsewhere = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

        let mut browsing = Browsing {
            state: Some(BrowserState::new(asked_dir.path().to_path_buf(), dest)),
            worker: None,
        };
        browsing.request(BrowseRequest::CollectTree {
            roots: vec![asked_dir.path().to_path_buf()],
            dest,
        });
        // Moved to a different directory rather than closed: the captured
        // destination must not follow the browser there. A different
        // destination proves the point: even the new browser's own capture
        // does not retroactively touch the tree already in flight.
        let elsewhere_dest = PlaylistId::from_raw_for_tests(9);
        browsing.state = Some(BrowserState::new(
            elsewhere.path().to_path_buf(),
            elsewhere_dest,
        ));

        let trees = poll_for_a_tree(&mut browsing);
        assert_eq!(trees.len(), 1, "{trees:?}");
        assert_eq!(trees[0].dest, dest);
        assert_eq!(trees[0].items, vec![file]);
        assert_eq!(
            browsing.state.as_ref().map(|state| &state.cwd),
            Some(&elsewhere.path().to_path_buf()),
            "the browser is still looking at the other directory"
        );
    }
}
