//! The `tenuto play` application: argument-to-source resolution, terminal
//! setup, and the key-driven status loop around [`EngineHandle`].

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute};

use crate::application::profile::{OpenedState, open_state};
use crate::application::runtime::{AppCommand, FlushReport, PlayerRuntime, RuntimeParts};
use crate::application::source::resolve_source;
use crate::application::transport::PlaybackPhase;
use crate::application::view::NowPlaying;
use crate::cli::{self, CliCommand};
use crate::clock::{Clock, SystemClock};
use crate::error::LifecycleError;
use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::lifecycle::RunOutcome;
use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::input::InputReader;
use crate::lifecycle::signals::ShutdownSignals;
use crate::media::capabilities::SeekSupport;
use crate::media::display::{fit_to_width, format_hms};
use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::persistence::store::StateStore;
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::prepare::{PrepareContext, prepare};
use crate::playback::state::PlaybackState;
use crate::volume::Volume;
use unicode_width::UnicodeWidthStr;

/// What [`run`] returns (design doc §6.5). Both arms are
/// `transparent`, so `main.rs`'s `{error}` and `?error` keep printing the
/// concrete failure rather than a wrapper that says nothing.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Playback(#[from] crate::playback::error::PlaybackError),
    #[error(transparent)]
    Feed(#[from] crate::feed::error::FeedError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

/// Re-exported so a test can drive the exact key routing this file's own key
/// loop uses, with no tty and no crossterm event in the loop at all.
pub use crate::application::seek::KeyRouter;

const SEEK_STEP_SECS: i64 = 10;
const VOLUME_STEP: f32 = 0.05;
const HELP_LINE: &str =
    "space pause · ←/→ seek 10s · Home restart · -/+ volume · s stop · p play · q quit";

/// Runs the parsed CLI to completion (design doc §6.5).
///
/// Both `play` forms resolve their `(MediaId, SourceLocation)` pair *before*
/// entering the shared playback body: one positional through the existing
/// [`resolve_source`], two through [`crate::library::resolve_episode`].
/// `resolve_source` itself is unchanged; it gained a sibling. Everything
/// else dispatches to [`crate::commands`], which owns every line this
/// program prints for a feed command, the one synchronous bridge into the
/// HTTP runtime, and the exit status a partial failure has to carry.
pub fn run(cli: cli::Cli) -> Result<RunOutcome, AppError> {
    // A bare `tenuto` opens the player. The defaults are the ones
    // `tenuto tui` applies when neither flag is given.
    let Some(command) = cli.command else {
        return crate::tui::run(crate::tui::TuiOptions {
            mouse: crate::tui::MouseMode::default(),
            artwork: crate::tui::images::ArtworkMode::default(),
        })
        .map_err(AppError::from);
    };
    match command {
        CliCommand::Play {
            source,
            index: None,
            probe_only,
        } => {
            if probe_only {
                return run_probe_only(&source)
                    .map(|()| RunOutcome::Completed)
                    .map_err(Into::into);
            }
            let (media, location) = resolve_source(&source)?;
            run_resolved(media, location)
        }
        CliCommand::Play {
            source: slug,
            index: Some(index),
            probe_only,
        } => {
            // No `EngineHandle`, no `AudioOutput` and no `HttpService` exist
            // yet, which is what keeps `NotPlayable` (§6.4) a resolution
            // failure rather than a playback one.
            let crate::application::runtime::LibraryStores {
                subscriptions: subs,
                cache,
                ..
            } = crate::application::runtime::LibraryStores::platform()?;
            let (media, location) =
                crate::library::resolve_episode(&subs, &cache, &slug, index.get())?;
            if probe_only {
                // §6.3: the probe applies *after* resolution, over the
                // enclosure this episode actually points at. The `RemoteUrl`
                // identity `run_probe_only` derives internally is never
                // persisted — the probe writes no state at all — so the
                // podcast identity resolved above is not diluted by it.
                if let SourceLocation::Http(url) = &location {
                    return run_probe_only(url.as_str())
                        .map(|()| RunOutcome::Completed)
                        .map_err(Into::into);
                }
                return Err(crate::feed::error::FeedError::Malformed {
                    detail: "podcast cache contained a non-HTTP source".into(),
                }
                .into());
            }
            // The podcast `MediaId` travels on unchanged: what is played is
            // the enclosure, what is checkpointed is the episode.
            run_resolved(media, location)
        }
        CliCommand::Tui { mouse, artwork } => {
            crate::tui::run(crate::tui::TuiOptions { mouse, artwork }).map_err(AppError::from)
        }
        command => crate::commands::run(command)
            .map(|()| RunOutcome::Completed)
            .map_err(Into::into),
    }
}

/// Signals are installed before anything else that could fail (source
/// resolution is the caller's job, done before this is ever called), the
/// exclusive profile lock is taken next, and only then is `state.json`
/// loaded: a rejected source is reported before a second player would ever
/// be told the profile is contended, and no process reads or writes state
/// that another player still holds. The lock is bound here, not in
/// [`run_resolved_locked`], so it stays held for that whole call and is only
/// released once this function returns — after the final flush.
///
/// A signal recorded during the run wins over whatever `run_resolved_locked`
/// itself returned, playback error included (design doc M5 §6.5) — the
/// listener that recorded it neither loaded state nor rendered anything, so
/// a signal arriving the same instant as, say, a device fault is still
/// reported as the shutdown it actually was. `signals.outcome()` is read
/// before `close()`, which only tears the listener down and cannot change
/// what it already recorded.
fn run_resolved(media: MediaId, location: SourceLocation) -> Result<RunOutcome, AppError> {
    use crate::lifecycle::lock::{LockError, ProfileLock};

    let signals = ShutdownSignals::install().map_err(LifecycleError::Signals)?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state_path = StateStore::platform_path()
        .map_err(|_| LifecycleError::from(LockError::NoStateDirectory))?;
    let _lock = ProfileLock::acquire(&state_path).map_err(LifecycleError::from)?;
    let store = StateStore::new(state_path, Arc::clone(&clock));

    let outcome = run_resolved_locked(media, location, clock, store, &signals);
    let signalled = signals.outcome();
    signals.close();
    match signalled {
        RunOutcome::Signalled(number) => Ok(RunOutcome::Signalled(number)),
        RunOutcome::Completed => outcome.map(|()| RunOutcome::Completed).map_err(Into::into),
    }
}

/// The playback body over the player runtime (M9.3): open the state, play
/// the media detached from every playlist, and drive keys, the runtime and
/// the status row until a quit, a failure, a signal or (with no tty) the end
/// of the track. It receives an already-locked `StateStore` from
/// [`run_resolved`], which keeps `platform_path` down to one caller (§13).
fn run_resolved_locked(
    media: MediaId,
    location: SourceLocation,
    clock: Arc<dyn Clock>,
    store: StateStore,
    signals: &ShutdownSignals,
) -> Result<(), PlaybackError> {
    let loaded = store.load();
    let OpenedState {
        session,
        writer,
        persisting,
        ..
    } = open_state(store, loaded, &clock);
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session,
        writer,
        persisting,
        clock,
        engine_factory: Box::new(EngineHandle::spawn_for_environment),
        // A podcast episode arrives already resolved; nothing else here
        // reads the library.
        library: None,
        http_limits: Limits::default(),
        metadata_probe: None,
        hook: TestHook::None,
    });
    runtime.play_detached(media, location);
    // A load that failed before it reached the engine (the HTTP service
    // would not start, say) is reported now, before the terminal goes raw and
    // before `Loading` is printed.
    let view = runtime.view();
    if let Some(failed) = outcome(
        view.phase,
        view.now_playing.as_ref(),
        view.status.as_deref(),
    ) {
        report_flush(runtime.shutdown());
        return failed;
    }

    // Entered only now (R5, Ruling 1): every fallible step above can still
    // fail before a single key is read, which is what keeps a rejected
    // source reportable with no raw terminal. `None` means there is no tty —
    // the CI case — and such a session simply reads no keys.
    let raw = RawModeGuard::enable();

    // Raw mode does not translate `\n`. §5: the application shows Loading
    // while preparation is in flight and remains able to stop or quit.
    print!("Loading \u{2026}\r\n");
    let _ = std::io::stdout().flush();

    let outcome = loop {
        // Checked first in the pass (Ruling 2): a recorded signal breaks
        // straight into the shutdown rather than waiting for a render.
        if signals.requested() {
            break Ok(());
        }
        match next_key(&runtime, keys(&raw), signals) {
            Key::Quit => break Ok(()),
            Key::Command(command) => runtime.handle(command),
            Key::None => {}
        }
        runtime.pump();
        let view = runtime.view();
        if let Some(outcome) = outcome(
            view.phase,
            view.now_playing.as_ref(),
            view.status.as_deref(),
        ) {
            break outcome;
        }
        // Nothing is drawn until the track has loaded: `Loading` stands.
        if let Some(now) = view.now_playing.as_ref().filter(|now| now.loaded)
            && let Err(error) = render(now, view.live, view.volume)
        {
            // D18: a terminal write failure is not a reason to skip the final
            // checkpoint, so it becomes the outcome instead of returning.
            break Err(error);
        }
        // With no controlling terminal no key can ever end this run, so the
        // end of its one track does. A signal still ends it sooner.
        if raw.is_none() && view.phase == PlaybackPhase::Ended {
            break Ok(());
        }
    };

    // Restore the terminal before waiting on the engine and the disk, and
    // before the caller prints a diagnostic on `outcome` (Ruling 1).
    drop(raw);
    report_flush(runtime.shutdown());
    outcome
}

/// What ends the run: a load that failed, or a track that failed after it
/// loaded — the runtime shows the latter as `Stopped` with a `Failed` state.
/// `None` keeps the run going.
fn outcome(
    phase: PlaybackPhase,
    now: Option<&NowPlaying>,
    status: Option<&str>,
) -> Option<Result<(), PlaybackError>> {
    let failed = phase == PlaybackPhase::LoadFailed
        || now.is_some_and(|now| now.state == PlaybackState::Failed);
    failed.then(|| {
        Err(PlaybackError::Failed(
            status.unwrap_or("playback failed").to_owned(),
        ))
    })
}

/// What one key wait produced.
enum Key {
    Quit,
    Command(AppCommand),
    None,
}

/// One wait for a key, capped by the runtime's seek-burst deadline so an
/// arrow burst flushes on time.
///
/// Keys come from `input`'s reader thread, never from crossterm on this
/// thread: on a hung-up terminal crossterm's poll never returns, and the loop
/// must still see the hangup's SIGHUP and flush.
///
/// With no raw terminal there are no keys, and calling into crossterm anyway
/// fails outright rather than answering "nothing ready". The wait instead
/// races the shutdown signal's wake, so a signal during a stalled open is
/// seen on the very next pass.
fn next_key(
    runtime: &PlayerRuntime,
    input: Option<&InputReader>,
    signals: &ShutdownSignals,
) -> Key {
    let budget = runtime.poll_budget(Duration::from_millis(100));
    let Some(input) = input else {
        let _ = signals.wake().recv_timeout(budget);
        return Key::None;
    };
    match input.next(budget) {
        Ok(Some(Event::Key(key))) => to_command(key),
        Ok(_) => Key::None,
        // The input stream ended or failed: shut down as cleanly as `q`.
        Err(_) => Key::Quit,
    }
}

/// The key reader of a raw terminal, if there is one.
fn keys(raw: &Option<RawModeGuard>) -> Option<&InputReader> {
    raw.as_ref().map(|raw| &raw.input)
}

/// §5/H15: opens and classifies `source` on the calling thread. No
/// `EngineHandle`, no `AudioOutput` and no `StateStore` are constructed —
/// this reads no playback state and writes none.
fn run_probe_only(source: &str) -> Result<(), PlaybackError> {
    let (_, location) = resolve_source(source)?;
    let http = match &location {
        SourceLocation::Http(_) => Some(HttpService::spawn(Limits::default())?),
        SourceLocation::LocalPath(_) => None,
    };
    let context = PrepareContext {
        http,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(InertHook),
        limits: Limits::default(),
        expected: None,
    };
    let mut prepared = prepare(&location, &context)?;

    // Preparation alone stops at `SeekSupport::Unknown` for every remote
    // source (§6): performing the trial seek here, before printing, is what
    // lets the probe tell "unresolved" from "unsupported" apart. A probe that
    // printed `Unknown` would only be reporting its own incuriosity as a
    // property of the recording.
    if prepared.capabilities.seek == SeekSupport::Unknown {
        let seeked = prepared
            .source
            .seek_refined(Duration::ZERO, None, &mut || false)
            .is_ok();
        if seeked {
            prepared.source.note_demuxer_proven();
            prepared.capabilities = prepared.source.capabilities();
        }
    }

    // Decoder metadata is untrusted, local files included: escaped the way
    // playback's status row and the feed listings escape it.
    let title = crate::telemetry::displayable(
        prepared
            .source
            .metadata()
            .title
            .as_deref()
            .unwrap_or("(untitled)"),
    );
    println!(
        "{title} {rate} Hz {channels} ch {duration:?} continuity={continuity:?} seek={seek:?} resume={resume:?}",
        rate = prepared.source.sample_rate(),
        channels = prepared.source.channels(),
        duration = prepared.source.metadata().duration,
        continuity = prepared.capabilities.continuity,
        seek = prepared.capabilities.seek,
        resume = prepared.capabilities.resume_capability(),
    );
    Ok(())
}

/// A `WaitHook` with nothing to do. `--probe-only` runs `prepare` on the
/// calling thread with no worker behind it, so the hook a blocked read would
/// service has no progress to publish and no freeze to act on.
struct InertHook;

impl WaitHook for InertHook {
    fn service(&self) {}
}

fn report_flush(report: FlushReport) {
    match report {
        FlushReport::Written => tracing::debug!("final checkpoint written"),
        FlushReport::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        FlushReport::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
        FlushReport::Disabled => {
            tracing::debug!("persistence is disabled for this session; no checkpoint was written");
        }
    }
}

/// Installs raw mode and restores it on drop. The play loop drops it
/// explicitly, so the writer's shutdown bound is never spent with the terminal
/// still raw; the `Drop` covers a panic, which is the only way out of the loop
/// that does not reach that line. It owns the thread that reads keys while the terminal
/// is raw.
struct RawModeGuard {
    input: InputReader,
}

/// How many keys may wait unread before the reader stops reading.
const KEY_BACKLOG: usize = 64;

impl RawModeGuard {
    /// `None` when there is no controlling terminal — `enable_raw_mode` fails
    /// for want of a tty, which is the CI case this type exists to keep out
    /// of raw-mode restoration's way (Ruling 1). Such a session reads no
    /// keys; `Loading` and any failure still print, and nothing here is left
    /// toggled for the loop to restore. A key reader thread that cannot be
    /// started leaves the terminal as it was and the session the same way.
    fn enable() -> Option<Self> {
        if let Err(error) = crossterm::terminal::enable_raw_mode() {
            tracing::debug!(%error, "no controlling terminal; running without raw mode");
            return None;
        }
        match InputReader::spawn(KEY_BACKLOG) {
            Ok(input) => Some(Self { input }),
            Err(error) => {
                let _ = crossterm::terminal::disable_raw_mode();
                tracing::warn!(%error, "cannot read keys; running without raw mode");
                None
            }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn to_command(key: KeyEvent) -> Key {
    if key.kind != KeyEventKind::Press {
        return Key::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Key::Quit;
    }
    Key::Command(match key.code {
        KeyCode::Char(' ') => AppCommand::PlayPause,
        KeyCode::Left => AppCommand::SeekBy(-SEEK_STEP_SECS),
        KeyCode::Right => AppCommand::SeekBy(SEEK_STEP_SECS),
        KeyCode::Home => AppCommand::Restart,
        // `+` sits behind Shift on the `=` key on most layouts, while `-` does
        // not, so binding only `+` makes the two directions asymmetric to press.
        // Accept the unshifted and shifted spelling of each.
        KeyCode::Char('-' | '_') => AppCommand::AdjustVolume(-VOLUME_STEP),
        KeyCode::Char('+' | '=') => AppCommand::AdjustVolume(VOLUME_STEP),
        KeyCode::Char('s') => AppCommand::Stop,
        KeyCode::Char('p') => AppCommand::Play,
        KeyCode::Char('q') => return Key::Quit,
        _ => return Key::None,
    })
}

fn render(now: &NowPlaying, live: bool, volume: Volume) -> Result<(), PlaybackError> {
    let mut out = std::io::stdout();
    // The frame is two rows that are repainted in place: clear a row, print,
    // then step back up one. That arithmetic only holds while each row
    // occupies exactly one physical line, so anything wider than the terminal
    // has to be cut before it is printed (see `fit_to_width`).
    let width = crossterm::terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(80);
    execute!(
        out,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print({
            let (name, fields) = status_parts(now, live, volume);
            fit_status(&name, &fields, width)
        }),
        Print("\r\n"),
        Clear(ClearType::CurrentLine),
        Print(fit_to_width(HELP_LINE, width)),
        cursor::MoveToColumn(0),
        cursor::MoveUp(1),
    )?;
    Ok(())
}

/// The status row as its two halves: the name, and the fields after it.
///
/// They are kept apart so `render` can decide which to shorten. The name
/// arrives escaped: the runtime runs every title through `displayable`, the
/// same policy the listings apply to feed titles, since an episode title is
/// decoder metadata from the network.
fn status_parts(now: &NowPlaying, live: bool, volume: Volume) -> (String, String) {
    let name = now.title.clone();
    let position = format_hms(now.position);
    // Two independent marks for two independent facts (§3): a degraded
    // quality says the played-so-far estimate may be off, while `~est` says
    // the absolute position itself was never decoder-confirmed. Neither
    // implies the other, so both may appear together.
    let mut suffix = String::new();
    if now.degraded {
        suffix.push_str(" ~");
    }
    if now.estimated_position {
        suffix.push_str(" ~est");
    }
    let duration = if live {
        "live".to_owned()
    } else {
        now.duration
            .map(|duration| format_hms(duration.value))
            .unwrap_or_else(|| "--:--:--".to_string())
    };
    // §11: unresolved and unsupported are different facts about the same
    // `SeekSupport`, and an HTTP transport must never be reported in a way
    // that reads as live radio — neither note ever replaces the duration
    // fallback above, which stays exactly what it always meant: an unknown
    // duration, nothing about seeking.
    let seek_note = match now.seek {
        Some(SeekSupport::Unknown) => " seek?",
        Some(SeekSupport::Unsupported) => " no-seek",
        _ => "",
    };
    let mut label = now.state.label().to_string();
    // A detail of Playing, never a state of its own (§11): this never grows
    // a fifth word beside idle/loading/playing/paused/stopped/ended/failed.
    if now.buffering && now.state == PlaybackState::Playing {
        label.push_str(" buffering");
    }
    let fields = format!(
        " [{label}]{seek_note} {position}{suffix} / {duration}  vol {}%",
        volume.percent(),
    );
    (name, fields)
}

/// Fit a status row into `width` columns by shortening the name first.
///
/// State, position, duration and volume are what the row is read for; the name
/// only says what is playing. So the fields keep their full width while they
/// fit at all, and the name gives way — cutting from the right of the whole
/// row instead kept a hundred characters of identifier and discarded the
/// position. If even the fields alone are wider than the terminal, the row is
/// still cut rather than wrapped, because a wrapped row is what breaks the
/// in-place repaint.
fn fit_status(name: &str, fields: &str, width: usize) -> String {
    let fields_width = UnicodeWidthStr::width(fields);
    if fields_width >= width {
        return fit_to_width(&format!("{name}{fields}"), width);
    }
    format!("{}{fields}", fit_to_width(name, width - fields_width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provenance::PositionProvenance;
    use crate::playlist::queue::{DisplayDuration, DurationSource};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn volume_step(code: KeyCode) -> Option<f32> {
        match to_command(press(code)) {
            Key::Command(AppCommand::AdjustVolume(delta)) => Some(delta),
            _ => None,
        }
    }

    #[test]
    fn volume_up_does_not_require_shift() {
        // `+` shares a key with `=` on most layouts, so binding only `+` makes
        // turning the volume up need Shift while turning it down does not.
        let shifted = volume_step(KeyCode::Char('+')).expect("+ raises volume");
        let unshifted = volume_step(KeyCode::Char('=')).expect("= raises volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted > 0.0);
    }

    #[test]
    fn volume_down_accepts_both_spellings_of_its_key() {
        let unshifted = volume_step(KeyCode::Char('-')).expect("- lowers volume");
        let shifted = volume_step(KeyCode::Char('_')).expect("_ lowers volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted < 0.0);
    }

    #[test]
    fn the_two_directions_are_symmetric_to_press() {
        assert_eq!(
            volume_step(KeyCode::Char('=')),
            volume_step(KeyCode::Char('-')).map(|step| -step)
        );
    }

    #[test]
    fn q_and_ctrl_c_end_the_run() {
        assert!(matches!(to_command(press(KeyCode::Char('q'))), Key::Quit));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(to_command(ctrl_c), Key::Quit));
    }

    // ------------------------------------------------------------- outcome

    fn now_in(state: PlaybackState) -> NowPlaying {
        NowPlaying {
            loaded: true,
            state,
            ..NowPlaying::unloaded()
        }
    }

    #[test]
    fn a_load_failure_ends_the_run_with_its_message() {
        let now = now_in(PlaybackState::Failed);
        assert!(matches!(
            outcome(PlaybackPhase::LoadFailed, Some(&now), Some("cannot open media")),
            Some(Err(PlaybackError::Failed(message))) if message == "cannot open media"
        ));
    }

    #[test]
    fn a_failure_after_loading_ends_the_run_with_its_message() {
        // The runtime maps a mirrored `Failed` to the `Stopped` phase; the
        // state is what says the track failed.
        let now = now_in(PlaybackState::Failed);
        assert!(matches!(
            outcome(PlaybackPhase::Stopped, Some(&now), Some("the server went quiet")),
            Some(Err(PlaybackError::Failed(message))) if message == "the server went quiet"
        ));
    }

    #[test]
    fn playing_paused_stopped_and_ended_do_not_end_the_run_by_themselves() {
        for (phase, state) in [
            (PlaybackPhase::Loading, PlaybackState::Loading),
            (PlaybackPhase::Playing, PlaybackState::Playing),
            (PlaybackPhase::Paused, PlaybackState::Paused),
            (PlaybackPhase::Stopped, PlaybackState::Stopped),
            (PlaybackPhase::Ended, PlaybackState::Ended),
        ] {
            assert!(
                outcome(phase, Some(&now_in(state)), None).is_none(),
                "{phase:?}"
            );
        }
        assert!(outcome(PlaybackPhase::Loading, None, None).is_none());
    }

    // ---------------------------------------------------------- status row

    fn status_line(now: &NowPlaying, live: bool) -> String {
        let (name, fields) = status_parts(now, live, Volume::new(0.8));
        format!("{name}{fields}")
    }

    fn with_seek(seek: SeekSupport) -> NowPlaying {
        NowPlaying {
            seek: Some(seek),
            ..now_in(PlaybackState::Playing)
        }
    }

    #[test]
    fn a_resumed_position_and_the_volume_are_on_the_row() {
        let now = NowPlaying {
            position: Duration::from_secs(93),
            duration: Some(DisplayDuration {
                value: Duration::from_secs(300),
                source: DurationSource::Decoded(PositionProvenance::Established),
            }),
            ..now_in(PlaybackState::Paused)
        };
        let line = status_line(&now, false);
        assert!(line.contains("00:01:33 / 00:05:00"), "{line}");
        assert!(line.contains("vol 80%"), "{line}");
    }

    /// The runtime escapes the title (`displayable`) before the row sees it;
    /// the row carries it unchanged rather than escaping it twice.
    #[test]
    fn the_row_carries_the_runtime_title_unchanged() {
        let now = NowPlaying {
            title: r"Радио-Т\u{1b}[2J1030".to_owned(),
            ..now_in(PlaybackState::Playing)
        };
        let (name, _) = status_parts(&now, false, Volume::FULL);
        assert_eq!(name, now.title);
    }

    #[test]
    fn a_live_source_reads_live_not_as_a_duration() {
        let line = status_line(&with_seek(SeekSupport::Unsupported), true);
        assert!(line.contains("live"), "{line}");
    }

    #[test]
    fn an_unresolved_seek_capability_is_marked_distinctly_from_unsupported() {
        let unresolved = status_line(&with_seek(SeekSupport::Unknown), false);
        let unsupported = status_line(&with_seek(SeekSupport::Unsupported), false);
        assert!(unresolved.contains("seek?"), "{unresolved}");
        assert!(!unresolved.contains("no-seek"), "{unresolved}");
        assert!(unsupported.contains("no-seek"), "{unsupported}");
    }

    #[test]
    fn a_seekable_source_carries_no_seek_note_at_all() {
        let line = status_line(&with_seek(SeekSupport::Native), false);
        assert!(!line.contains("seek?"), "{line}");
        assert!(!line.contains("no-seek"), "{line}");
    }

    #[test]
    fn buffering_is_shown_only_as_a_detail_of_playing() {
        let mut now = NowPlaying {
            buffering: true,
            ..now_in(PlaybackState::Playing)
        };
        assert!(status_line(&now, false).contains("buffering"));
        // Never a state of its own: a paused session servicing a blocked read
        // does not print "buffering" under a label that is not Playing.
        now.state = PlaybackState::Paused;
        assert!(!status_line(&now, false).contains("buffering"));
    }

    /// §11: `buffering` must never be derived from a degraded quality.
    #[test]
    fn degraded_quality_does_not_imply_buffering() {
        let now = NowPlaying {
            degraded: true,
            ..now_in(PlaybackState::Playing)
        };
        let line = status_line(&now, false);
        assert!(!line.contains("buffering"), "{line}");
        assert!(line.contains(" ~"), "{line}");
    }

    #[test]
    fn an_estimated_position_is_marked() {
        let now = NowPlaying {
            estimated_position: true,
            ..now_in(PlaybackState::Playing)
        };
        assert!(status_line(&now, false).contains("~est"));
    }

    // ----------------------------------------------------------- fitting

    /// The regression the first fix introduced: cutting the whole row from the
    /// right kept ~110 characters of identifier and discarded the position,
    /// which is the one thing a listener reads the row for.
    #[test]
    fn a_long_name_gives_way_so_the_position_stays_visible() {
        let name = "x".repeat(200);
        let fields = " [playing] 00:00:04 / 03:07:31  vol 80%";
        let row = fit_status(&name, fields, 80);
        assert_eq!(row.chars().count(), 80);
        assert!(
            row.ends_with(fields),
            "the fields must survive whole: {row}"
        );
        assert!(row.contains('…'), "the name is what was cut: {row}");
    }

    #[test]
    fn a_row_narrower_than_its_own_fields_is_still_cut_not_wrapped() {
        let row = fit_status(
            "Радио-Т 1030",
            " [playing] 00:00:04 / 03:07:31  vol 80%",
            12,
        );
        assert_eq!(row.chars().count(), 12);
    }

    /// The name budget is in columns: a CJK name is cut so the whole row,
    /// fields included, still fits the terminal width.
    #[test]
    fn a_wide_name_is_cut_by_columns_so_the_row_still_fits() {
        let fields = " [playing] 00:00:04 / 03:07:31  vol 80%";
        let row = fit_status(&"界".repeat(40), fields, 60);
        assert!(row.ends_with(fields), "{row}");
        assert!(UnicodeWidthStr::width(row.as_str()) <= 60, "{row}");
        assert!(UnicodeWidthStr::width(row.as_str()) >= 59, "{row}");
    }

    #[test]
    fn a_row_that_fits_keeps_its_whole_name() {
        let fields = " [playing] 00:00:04 / 03:07:31  vol 80%";
        assert_eq!(
            fit_status("Радио-Т 1030", fields, 80),
            format!("Радио-Т 1030{fields}")
        );
    }
}
