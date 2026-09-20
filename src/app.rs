//! The `tenuto play` application: argument-to-source resolution, terminal
//! setup, and the key-driven status loop around [`EngineHandle`].

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute};

use crate::application::runtime::{FlushReport, classify_flush, shut_down_engine};
use crate::application::source::resolve_source;
use crate::cli::{self, CliCommand};
use crate::clock::{Clock, SystemClock};
use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::lifecycle::RunOutcome;
use crate::lifecycle::input::InputReader;
use crate::lifecycle::signals::ShutdownSignals;
use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use crate::media::display::{display_name, episode_name, fit_to_width, format_hms};
use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::persistence::store::{LoadReason, QueueBackup, StateStore};
use crate::persistence::writer::{DisabledSink, ShutdownOutcome, StateSink, WriterHandle};
use crate::playback::command::{LoadRequestId, PlaybackCommand, ResumeIntent};
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::{PlaybackEvent, Progress};
use crate::playback::prepare::{PrepareContext, prepare};
use crate::playback::provenance::PositionProvenance;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;
use crate::session::{Action, LoadTarget, Session, resume_intent_for};
use unicode_width::UnicodeWidthStr;

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
pub fn run(cli: cli::Cli) -> Result<RunOutcome, crate::error::AppError> {
    // A bare `tenuto` opens the player. The defaults are the ones
    // `tenuto tui` applies when neither flag is given.
    let Some(command) = cli.command else {
        return crate::tui::run(crate::tui::TuiOptions {
            mouse: cli::MouseMode::default(),
            artwork: cli::ArtworkMode::default(),
        });
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
            let (subs, cache) = crate::commands::platform_subscription_stores()?;
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
            crate::tui::run(crate::tui::TuiOptions { mouse, artwork })
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
/// released once this function returns — after `finish`'s `report_flush`.
///
/// A signal recorded during the run wins over whatever `run_resolved_locked`
/// itself returned, playback error included (design doc M5 §6.5) — the
/// listener that recorded it neither loaded state nor rendered anything, so
/// a signal arriving the same instant as, say, a device fault is still
/// reported as the shutdown it actually was. `signals.outcome()` is read
/// before `close()`, which only tears the listener down and cannot change
/// what it already recorded.
fn run_resolved(
    media: MediaId,
    location: SourceLocation,
) -> Result<RunOutcome, crate::error::AppError> {
    use crate::error::LifecycleError;
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

/// The shared playback body: persistence open, engine assembly, resume,
/// session, both key loops and the shutdown. It receives an already-locked
/// `StateStore` from [`run_resolved`] rather than resolving one of its own,
/// which is what keeps `platform_path` down to exactly one caller in the
/// program (§13).
fn run_resolved_locked(
    media: MediaId,
    location: SourceLocation,
    clock: Arc<dyn Clock>,
    store: StateStore,
    signals: &ShutdownSignals,
) -> Result<(), PlaybackError> {
    // Persistence opens before the engine: the resume candidate is an
    // argument to the load, and the restored volume is a command that
    // precedes it.
    let Persistence {
        mut session,
        writer,
        resume,
        volume,
        persisting,
    } = open_persistence(store, &media, &clock);

    // Built before the engine spawns: `EngineHandle::set_http`'s default is
    // `None`, which fails every remote `Load` with "no HTTP service in this
    // session" — installing it before the first command reaches the worker
    // is what makes that failure mode unreachable for a source this CLI
    // itself resolved as remote.
    let http = match &location {
        SourceLocation::Http(_) => Some(HttpService::spawn(Limits::default())?),
        SourceLocation::LocalPath(_) => None,
    };

    let engine = EngineHandle::spawn_for_environment();
    if let Some(service) = http {
        engine.set_http(Some(service));
    }

    // One load for the whole run, registered before anything is sent so the
    // `Loaded` it produces has a token `session` recognizes (M5 §6). `Busy`
    // cannot happen — this is the only load this session has ever asked
    // for — so a session-ending error is the honest way to report it anyway.
    let request = session
        .register_load(LoadTarget::Legacy, &media)
        .map_err(|error| PlaybackError::Failed(format!("cannot register the load: {error:?}")))?;
    for command in resume_commands(media, location, resume, volume, request) {
        if matches!(command, PlaybackCommand::Load { .. }) {
            if engine.commands().send(command).is_err() {
                session.retract_load(request);
            }
        } else {
            engine.commands().send(command).ok();
        }
    }

    // Entered only now (R5, Ruling 1): every fallible step above can still
    // fail before a single key is read, which is what keeps a rejected
    // source reportable with no raw terminal. `None` means there is no tty —
    // the CI case — and such a session simply reads no keys rather than
    // calling into crossterm, which has nothing to open and fails outright
    // rather than reporting "nothing ready".
    let raw = RawModeGuard::enable();
    let mut mirror = Mirror::default();
    let mut router = KeyRouter::new();

    // Raw mode does not translate `\n`. §5: "The application displays
    // Loading while preparation is in flight and remains able to stop or
    // quit" — this is the one line loop A renders, before the first `Loaded`
    // or `Failed` decides whether there is anything further to show.
    print!("Loading \u{2026}\r\n");
    let _ = std::io::stdout().flush();

    // Loop A: keys are read (Ctrl-C and `q` included) but nothing but the
    // line above is rendered. `Loaded` hands off to loop B; `Failed` or a
    // quit decides the run's outcome here, before loop B ever starts.
    let phase = loop {
        // Checked before anything else in the pass (Ruling 2): a signal
        // recorded here breaks straight into `finish` rather than waiting for
        // this pass's own render or event drain, neither of which a shutdown
        // needs.
        if signals.requested() {
            break Phase::Done(Ok(()));
        }
        if handle_keys(&engine, &mut router, &mut mirror, keys(&raw), signals) {
            break Phase::Done(Ok(()));
        }
        router.flush(&engine, Instant::now());

        let mut failure = None;
        let mut loaded = false;
        while let Ok(event) = engine.events().try_recv() {
            if let PlaybackEvent::Failed { message, .. } = &event {
                failure = Some(message.clone());
            }
            loaded |= matches!(event, PlaybackEvent::Loaded { .. });
            // `observe` borrows the event, so the mirror still consumes it.
            submit(&writer, session.observe(&event, clock.sample()));
            router.observe(&event);
            mirror.apply(event);
        }
        if let Some(message) = failure {
            break Phase::Done(Err(PlaybackError::Failed(message)));
        }
        if loaded {
            break Phase::Loaded;
        }
    };

    let outcome = match phase {
        Phase::Done(outcome) => outcome,
        // Loop B: the existing key/render/checkpoint loop.
        Phase::Loaded => loop {
            if signals.requested() {
                break Ok(());
            }
            if handle_keys(&engine, &mut router, &mut mirror, keys(&raw), signals) {
                break Ok(());
            }
            router.flush(&engine, Instant::now());

            let mut failure = None;
            while let Ok(event) = engine.events().try_recv() {
                if let PlaybackEvent::Failed { message, .. } = &event {
                    failure = Some(message.clone());
                }
                submit(&writer, session.observe(&event, clock.sample()));
                router.observe(&event);
                mirror.apply(event);
            }
            if let Some(message) = failure {
                break Err(PlaybackError::Failed(message));
            }

            // Render progress only when it belongs to the session the mirror is
            // showing. The keep-latest snapshot can otherwise overtake queued
            // lifecycle events and show one track's position under another's
            // title. A `Failed` event can carry a newer `session_rev` than the
            // snapshot published a tick earlier; the guard correctly skips
            // rendering the snapshot for that tick.
            let progress = engine.progress();
            submit(&writer, session.tick(&progress, clock.sample()));
            if progress.session_rev == mirror.session_rev {
                apply_progress(&mut mirror, &progress, router.is_seeking());
            }
            // A terminal write failure is not a reason to skip the final
            // checkpoint, so it becomes the loop's outcome instead of returning
            // from here and bypassing the flush path (D18).
            if let Err(error) = render(&mirror) {
                break Err(error);
            }
            // With no controlling terminal there is no key left to read that
            // could ever end this run (`q`/Ctrl-C need a tty), so a session
            // that has reached the end of its one track is otherwise stuck
            // rendering an unchanging frame forever. A signal still ends it
            // sooner; this is what ends it at all when none arrives.
            if raw.is_none() && mirror.state == PlaybackState::Ended {
                break Ok(());
            }
        },
    };

    finish(engine, session, writer, &clock, raw, persisting, outcome)
}

/// Copies the worker's view of the position into the mirror.
///
/// While a seek target stands - from the press that accumulated it until the
/// worker accounts for it - the position fields are deliberately left alone.
/// The display is already showing that target, while `Progress` goes on
/// reporting where playback actually is: before the flush because the worker
/// has not been asked to move yet, and after it because reopening a range
/// request and buffering take time. Copying it in either window would snap
/// the display back. `buffering` is unrelated to the target and keeps flowing
/// through.
///
/// Once the target is released, the landing arrives by this same path and
/// corrects the display if the prediction missed.
fn apply_progress(mirror: &mut Mirror, progress: &Progress, seeking: bool) {
    mirror.buffering = progress.buffering;
    if seeking {
        return;
    }
    mirror.position = progress.position;
    mirror.quality = progress.quality;
    mirror.provenance = progress.provenance;
}

/// What loop A decided: hand off to loop B once loaded, or the run is
/// already over (a quit, or a `Failed` before anything ever loaded).
enum Phase {
    Loaded,
    Done(Result<(), PlaybackError>),
}

/// Both loops break into this (Ruling 2): whichever loop produced `outcome`,
/// the shutdown sequence — interrupt, join, reconcile, restore the terminal,
/// flush — is one path rather than two, which is what keeps D18's "a
/// terminal write failure must not skip the final checkpoint" true
/// regardless of which loop hit it.
fn finish(
    engine: EngineHandle,
    mut session: Session,
    mut writer: WriterHandle,
    clock: &Arc<dyn Clock>,
    raw: Option<RawModeGuard>,
    persisting: bool,
    outcome: Result<(), PlaybackError>,
) -> Result<(), PlaybackError> {
    // Both loops have just stopped draining events, which is exactly the
    // backlog `shut_down_engine`'s out-of-band interrupt exists for; the
    // events neither loop drained are replayed before the forced snapshot.
    shut_down_engine(engine, &mut session, &writer, clock.as_ref());

    // Restore the terminal before waiting on the disk, and before returning
    // to a caller that will print a diagnostic on `outcome` — so the writer's
    // bound is never spent, and nothing is ever printed, with the terminal
    // still raw (Ruling 1).
    drop(raw);
    report_flush(writer.shutdown(), persisting);
    outcome
}

/// One pass of key handling, shared by both loops. Returns whether the loop
/// must stop: an explicit quit, Ctrl-C (`to_command` already maps it to
/// `Shutdown`), or the input stream ending or failing.
///
/// Keys come from `input`'s reader thread, never from crossterm on this
/// thread: on a hung-up terminal crossterm's poll never returns, and the loop
/// must still see the hangup's SIGHUP and flush.
///
/// With no raw terminal (`input` is `None`) there are no keys to read, and calling
/// into crossterm anyway does not answer "nothing ready" — with no tty to
/// open it fails outright (verified empirically against this crossterm
/// version), which would misreport a CI run with no controlling terminal as
/// someone having pressed `q`. Waiting out one tick and reporting nothing to
/// do is what actually matches "no keys", leaving the event drain in each
/// loop as the only thing such a session can still notice.
///
/// That wait is where a shutdown signal arriving during a stalled open (no
/// media loaded yet, nothing else in this loop pass blocks) would otherwise
/// sit unnoticed for up to a full tick: it races the ordinary sleep against
/// [`ShutdownSignals::wake`] rather than sleeping blind, so the signal ends
/// the wait the moment it arrives and the top-of-pass `requested()` check
/// sees it on the very next iteration.
fn handle_keys(
    engine: &EngineHandle,
    router: &mut KeyRouter,
    mirror: &mut Mirror,
    input: Option<&InputReader>,
    signals: &ShutdownSignals,
) -> bool {
    // Capped by whatever is sooner: the ordinary tick, or an open burst's own
    // deadline. Without the cap a window expiring just after a block began
    // would go unnoticed for a further full block.
    let budget = router.poll_budget(Instant::now(), Duration::from_millis(100));
    let Some(input) = input else {
        let _ = signals.wake().recv_timeout(budget);
        return false;
    };
    match input.next(budget) {
        Ok(Some(Event::Key(key))) => match to_command(key, mirror) {
            Some(PlaybackCommand::Shutdown) => true,
            Some(command) => {
                let optimistic = router.route(
                    engine,
                    matches!(
                        mirror.state,
                        PlaybackState::Playing | PlaybackState::Reconnecting
                    ),
                    mirror.position,
                    mirror.duration,
                    Instant::now(),
                    command,
                );
                // The jump that makes a single arrow press feel immediate
                // even though its fetch waits out the quiet window. It is
                // a prediction until the seek lands, so it is marked
                // `Estimated` and reaches only the display: the checkpoint
                // path reads `Progress`, never the mirror.
                if let Some(position) = optimistic {
                    mirror.position = position;
                    mirror.provenance = PositionProvenance::Estimated;
                }
                false
            }
            None => false,
        },
        Ok(_) => false,
        // The input stream ended or failed; there is nothing left to read
        // keys from, so shut down as cleanly as `q` would.
        Err(_) => true,
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
    let title = crate::commands::displayable(
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

/// §11's initial command sequence. Volume first: the engine accepts it with no
/// transport, and a transport created later adopts the stored gain — so the
/// restored level is in force from the first buffer rather than after it.
/// Nothing about the sequence is conditional; a session with no stored
/// candidate issues the same `Load`, with a start of zero.
fn resume_commands(
    media: MediaId,
    source: SourceLocation,
    resume: Option<ResumeIntent>,
    volume: Volume,
    request: LoadRequestId,
) -> [PlaybackCommand; 3] {
    // No entry is not itself a resume intent: the worker would decide
    // `NoEntry` from an absent `Candidate` anyway (§11), so this is the same
    // outcome without asking the worker to resolve one that was never
    // there. `resume_intent_for` has already decided, for whatever entry
    // there was, between `Candidate` (§11, unchanged) and
    // `EstimatedCandidate` (§4.3) — this function's only job left is the
    // "nothing at all" case.
    let resume = resume.unwrap_or(ResumeIntent::StartAt(Duration::ZERO));
    [
        PlaybackCommand::SetVolume(volume),
        PlaybackCommand::Load {
            request,
            media,
            source,
            resume,
        },
        PlaybackCommand::PlayLoaded { request },
    ]
}

struct Persistence {
    session: Session,
    writer: WriterHandle,
    /// The resume intent built from the stored entry for this media,
    /// unresolved against a duration — `resume_intent_for` (§4.2, §4.3)
    /// already decided between an established candidate and an estimated
    /// one, but a `Candidate`'s own position still needs a duration to
    /// validate against, and only the worker's own decode probe has one
    /// (Ruling 5). So `open_persistence` hands this onward rather than
    /// deciding a start position itself, which would mean opening the media
    /// twice for the same answer `decide_resume` gives either time.
    resume: Option<ResumeIntent>,
    volume: Volume,
    /// Whether anything this session submits can reach the disk. A disabled
    /// sink reports every write as a success, deliberately — the writer must
    /// not count a disable as a failure (D11) — so this is what keeps the
    /// shutdown log from claiming a write that never happened.
    persisting: bool,
}

fn open_persistence(store: StateStore, media: &MediaId, clock: &Arc<dyn Clock>) -> Persistence {
    let outcome = store.load();
    match &outcome.reason {
        LoadReason::Loaded => tracing::debug!(path = ?store.path(), "state restored"),
        LoadReason::Missing => tracing::debug!(path = ?store.path(), "no state yet"),
        LoadReason::Quarantined { moved_to } => {
            tracing::warn!(
                ?moved_to,
                "state file was unreadable and has been moved aside"
            );
        }
        LoadReason::QuarantineFailed => {
            tracing::warn!("state file is unreadable and could not be moved aside; not writing");
        }
        LoadReason::UnsupportedVersion { found } => {
            tracing::warn!(
                found,
                "state file is from a newer build; preserving it and not writing"
            );
        }
        LoadReason::Unreadable => {
            tracing::warn!("state file could not be read; preserving it and not writing");
        }
    }
    // §6: a repaired queue logs here too, distinct from the warning
    // `StateStore::load` already emits — that one is unconditional, this
    // one is what the TUI's status line (Task 17) will surface.
    if let Some(repair) = &outcome.queue_repair {
        match &repair.backup {
            QueueBackup::Saved(path) => {
                tracing::warn!(
                    fields = repair.reset.fields_reset(),
                    backup = ?path,
                    "queue data in the state file was reset"
                );
            }
            QueueBackup::Failed => {
                tracing::warn!(
                    fields = repair.reset.fields_reset(),
                    "queue data in the state file was reset; the backup could not be \
                     written, so persistence is disabled for this session"
                );
            }
        }
    }
    let (state, writable) = (outcome.state, outcome.writable);

    // No `ResumeDecision` is logged here any more: the decision needs a
    // duration, this call site has none, and logging one taken with
    // `duration: None` would misreport an ordinary resume as `Unvalidated`
    // every time. The disposition the worker reports on `Loaded` is what a
    // later task logs instead (Ruling 5).
    // `resume_intent_for` (§4.2, §4.3): a freshly loaded entry may carry
    // only an estimate with no established position at all, and that case
    // must not collapse into a fabricated `AtStart` — and, since Task 6's
    // fix round 1, an entry whose `estimated` field wins the §4.3
    // preference is resolved to `ResumeIntent::EstimatedCandidate` here
    // rather than the plain `Candidate` `resume_candidate` alone would
    // build.
    let resume = resume_intent_for(media, state.entry_for(media));
    let volume = state.volume();
    let sink: Box<dyn StateSink> = if writable {
        Box::new(store)
    } else {
        Box::new(DisabledSink)
    };

    Persistence {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        resume,
        volume,
        persisting: writable,
    }
}

fn submit(writer: &WriterHandle, action: Action) {
    if let Action::Submit { state, urgency } = action {
        writer.submit(state, urgency);
    }
}

fn report_flush(outcome: ShutdownOutcome, persisting: bool) {
    match classify_flush(outcome, persisting) {
        FlushReport::Written => tracing::debug!("final checkpoint written"),
        FlushReport::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        FlushReport::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
        FlushReport::Disabled => {
            tracing::debug!("persistence is disabled for this session; no checkpoint was written");
        }
    }
}

/// Installs raw mode and restores it on drop. `finish` drops it explicitly, so
/// the writer's shutdown bound is never spent with the terminal still raw; the
/// `Drop` covers a panic, which is the only way out of either loop that does
/// not reach that line. It owns the thread that reads keys while the terminal
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
    /// toggled for `finish` to restore. A key reader thread that cannot be
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

/// The application's read-only view of playback, rebuilt from the lossless
/// event stream and refreshed from the keep-latest progress snapshot.
struct Mirror {
    session_rev: u64,
    name: Option<String>,
    duration: Option<Duration>,
    state: PlaybackState,
    position: Duration,
    quality: PositionQuality,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3), read from `Progress`/`SeekCompleted` — never derived from
    /// `quality`, which is an unrelated fact.
    provenance: PositionProvenance,
    volume: Volume,
    /// §11: carried whole, rather than as a bare `SeekSupport`, so
    /// `status_line` can tell "unresolved" from "unsupported" apart. `None`
    /// until the first `Loaded`.
    capabilities: Option<MediaCapabilities>,
    /// True exactly while a source read is blocked on the network. A detail
    /// of `Playing`, never a state of its own (§11) — `status_line` is the
    /// only place this is read.
    buffering: bool,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            session_rev: 0,
            name: None,
            duration: None,
            state: PlaybackState::Idle,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            volume: Volume::default(),
            capabilities: None,
            buffering: false,
        }
    }
}

impl Mirror {
    fn apply(&mut self, event: PlaybackEvent) {
        match event {
            PlaybackEvent::Loaded {
                session_rev,
                media,
                metadata,
                capabilities,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.name = Some(match &media {
                    MediaId::PodcastEpisode { .. } => episode_name(metadata.title.as_deref()),
                    other => display_name(other),
                });
                self.duration = metadata.duration;
                self.capabilities = Some(capabilities);
                self.position = position;
                self.quality = PositionQuality::Exact;
                // `Loaded` does not carry its own provenance field (§3's
                // interfaces are scoped to `Progress`, `SeekCompleted` and
                // `MediaMetadata::duration`), so this hardcodes `Established`
                // even for a `ResumeIntent::EstimatedCandidate` launch, which
                // runs `seek_bounded` for the resume and can genuinely land
                // `Estimated` (`src/playback/engine.rs`'s resume-seek arm).
                // The mark this drives (` ~est`) is one tick late in that
                // case: `Progress` corrects `self.provenance` right after
                // (`:160` below), so the window is a single progress
                // interval, display-only — not fixed here for its own sake.
                self.provenance = PositionProvenance::Established;
                self.state = PlaybackState::Loading;
                // MINOR (final review): a fresh load starts with nothing
                // buffering. Display-only and unreachable under one load per
                // run, but leaving a stale `true` standing is a real bug,
                // not only a limitation.
                self.buffering = false;
            }
            PlaybackEvent::StateChanged {
                session_rev, state, ..
            } => {
                self.session_rev = session_rev;
                self.state = state;
            }
            PlaybackEvent::SeekCompleted {
                session_rev,
                actual,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = actual;
            }
            PlaybackEvent::SeekTargetStored {
                session_rev,
                target,
            } => {
                self.session_rev = session_rev;
                self.position = target;
            }
            PlaybackEvent::EndOfTrack {
                session_rev,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = position;
                self.state = PlaybackState::Ended;
            }
            PlaybackEvent::VolumeChanged {
                session_rev,
                volume,
            } => {
                self.session_rev = session_rev;
                self.volume = volume;
            }
            // G1: the only one of these three the mirror has anything to show
            // for. `restart()` lands at zero with no `SeekCompleted`, so this
            // is where the mirror's position learns it landed at all.
            PlaybackEvent::RestartEstablished {
                session_rev,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = position;
            }
            // §11: evidence that arrived after `Loaded` — an on-demand seek
            // probe resolving `Unknown` — updates the same field `Loaded`
            // itself seeds, so `status_line` reads the promotion to `Native`
            // the moment it is announced rather than only on the next load.
            PlaybackEvent::CapabilitiesChanged {
                session_rev,
                capabilities,
            } => {
                self.session_rev = session_rev;
                self.capabilities = Some(capabilities);
            }
            PlaybackEvent::DeviceRecovered { session_rev }
            | PlaybackEvent::SeekRejected { session_rev, .. }
            | PlaybackEvent::SeekCancelled { session_rev, .. }
            | PlaybackEvent::Warning { session_rev, .. }
            | PlaybackEvent::LoadCancelled { session_rev, .. }
            | PlaybackEvent::Failed { session_rev, .. } => {
                self.session_rev = session_rev;
            }
        }
    }
}

fn to_command(key: KeyEvent, mirror: &Mirror) -> Option<PlaybackCommand> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(PlaybackCommand::Shutdown);
    }
    match key.code {
        KeyCode::Char(' ') => Some(PlaybackCommand::TogglePause),
        KeyCode::Left => Some(PlaybackCommand::SeekBy(-SEEK_STEP_SECS)),
        KeyCode::Right => Some(PlaybackCommand::SeekBy(SEEK_STEP_SECS)),
        KeyCode::Home => Some(PlaybackCommand::Restart),
        // `+` sits behind Shift on the `=` key on most layouts, while `-` does
        // not, so binding only `+` makes the two directions asymmetric to press.
        // Accept the unshifted and shifted spelling of each.
        KeyCode::Char('-' | '_') => Some(PlaybackCommand::SetVolume(
            mirror.volume.adjusted(-VOLUME_STEP),
        )),
        KeyCode::Char('+' | '=') => Some(PlaybackCommand::SetVolume(
            mirror.volume.adjusted(VOLUME_STEP),
        )),
        KeyCode::Char('s') => Some(PlaybackCommand::Stop),
        KeyCode::Char('p') => Some(PlaybackCommand::Play),
        KeyCode::Char('q') => Some(PlaybackCommand::Shutdown),
        _ => None,
    }
}

fn render(mirror: &Mirror) -> Result<(), PlaybackError> {
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
            let (name, fields) = status_parts(mirror);
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

/// The status row joined, unfitted — what the row says before `render` fits
/// it to a terminal. Only the tests read the whole row as one string.
#[cfg(test)]
fn status_line(mirror: &Mirror) -> String {
    let (name, fields) = status_parts(mirror);
    format!("{name}{fields}")
}

/// The status row as its two halves: the name, and the fields after it.
///
/// They are kept apart so `render` can decide which to shorten. The name is
/// escaped here, at the row's formatting boundary: an episode title is decoder
/// metadata from the network, and the escaping is `commands::displayable`, the
/// same policy the listings apply to feed titles.
fn status_parts(mirror: &Mirror) -> (String, String) {
    let name = crate::commands::displayable(mirror.name.as_deref().unwrap_or("(no media)"));
    let position = format_hms(mirror.position);
    // Two independent marks for two independent facts (§3): a degraded
    // quality says the played-so-far estimate may be off, while `~est` says
    // the absolute position itself was never decoder-confirmed. Neither
    // implies the other, so both may appear together.
    let mut suffix = String::new();
    if mirror.quality == PositionQuality::Degraded {
        suffix.push_str(" ~");
    }
    if mirror.provenance == PositionProvenance::Estimated {
        suffix.push_str(" ~est");
    }
    let live = mirror
        .capabilities
        .is_some_and(|capabilities| capabilities.continuity == Continuity::Indefinite);
    let duration = if live {
        "live".to_owned()
    } else {
        mirror
            .duration
            .map(format_hms)
            .unwrap_or_else(|| "--:--:--".to_string())
    };
    // §11: unresolved and unsupported are different facts about the same
    // `SeekSupport`, and an HTTP transport must never be reported in a way
    // that reads as live radio — neither note ever replaces the duration
    // fallback above, which stays exactly what it always meant: an unknown
    // duration, nothing about seeking.
    let seek_note = match mirror.capabilities.map(|capabilities| capabilities.seek) {
        Some(SeekSupport::Unknown) => " seek?",
        Some(SeekSupport::Unsupported) => " no-seek",
        _ => "",
    };
    let mut label = mirror.state.label().to_string();
    // A detail of Playing, never a state of its own (§11): this never grows
    // a fifth word beside idle/loading/playing/paused/stopped/ended/failed.
    if mirror.buffering && mirror.state == PlaybackState::Playing {
        label.push_str(" buffering");
    }
    let fields = format!(
        " [{label}]{seek_note} {position}{suffix} / {duration}  vol {}%",
        mirror.volume.percent(),
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
    use crate::clock::FakeClock;
    use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use crate::media::id::{AbsolutePath, EpisodeKey, FeedId, NormalizedUrl};
    use crate::media::metadata::MediaMetadata;
    use crate::persistence::model::PersistedState;
    use crate::persistence::writer::Urgency;
    use crate::playback::checkpoint::PlaybackCheckpoint;
    use crate::playback::event::StartDisposition;
    use crate::resume::ResumeCandidate;
    use url::Url;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// Default volume is `FULL`, and `adjusted` clamps at 1.0, so a raise test
    /// has to start below full or it measures the clamp instead of the binding.
    fn volume_after(code: KeyCode, from: f32) -> Option<f32> {
        let mirror = Mirror {
            volume: Volume::new(from),
            ..Mirror::default()
        };
        match to_command(press(code), &mirror) {
            Some(PlaybackCommand::SetVolume(volume)) => Some(volume.as_gain()),
            _ => None,
        }
    }

    #[test]
    fn volume_up_does_not_require_shift() {
        // `+` shares a key with `=` on most layouts, so binding only `+` makes
        // turning the volume up need Shift while turning it down does not.
        let baseline = 0.5;
        let shifted = volume_after(KeyCode::Char('+'), baseline).expect("+ raises volume");
        let unshifted = volume_after(KeyCode::Char('='), baseline).expect("= raises volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted > baseline);
    }

    #[test]
    fn volume_down_accepts_both_spellings_of_its_key() {
        let baseline = 0.5;
        let unshifted = volume_after(KeyCode::Char('-'), baseline).expect("- lowers volume");
        let shifted = volume_after(KeyCode::Char('_'), baseline).expect("_ lowers volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted < baseline);
    }

    #[test]
    fn the_two_directions_are_symmetric_to_press() {
        // Whatever raises volume must be reachable with the same effort as what
        // lowers it: an unshifted key exists for each.
        assert!(volume_after(KeyCode::Char('='), 0.5).is_some());
        assert!(volume_after(KeyCode::Char('-'), 0.5).is_some());
    }

    /// §6.5's handoff: `run_resolved` plays whatever pair it is handed, and
    /// the identity in the `Load` it issues is that pair's own — never one
    /// re-derived from the source. For a podcast episode the two differ:
    /// what is played is the enclosure, what is checkpointed is the episode,
    /// and only the latter survives a feed moving its audio to another CDN.
    #[test]
    fn a_resolved_podcast_pair_loads_the_episode_identity_not_the_enclosure() {
        let enclosure = "https://cdn.example.org/987.mp3";
        let (feed, episode) = match (
            FeedId::new("0123456789abcdef0123456789abcdef".to_string()),
            EpisodeKey::resolve(Some("ep-987"), None, None),
        ) {
            (Ok(feed), Ok(episode)) => (feed, episode),
            (feed, episode) => panic!("literal identities must parse: {feed:?} {episode:?}"),
        };
        let media = MediaId::PodcastEpisode { feed, episode };
        let location = match Url::parse(enclosure) {
            Ok(url) => SourceLocation::Http(url),
            Err(error) => panic!("a literal URL must parse: {error}"),
        };

        let commands = resume_commands(
            media.clone(),
            location,
            None,
            Volume::FULL,
            LoadRequestId::from_raw(1),
        );
        match &commands[1] {
            PlaybackCommand::Load {
                media: loaded,
                source,
                resume,
                ..
            } => {
                assert_eq!(loaded, &media);
                assert!(matches!(source, SourceLocation::Http(url) if url.as_str() == enclosure));
                assert!(matches!(resume, ResumeIntent::StartAt(at) if *at == Duration::ZERO));
                match NormalizedUrl::parse(enclosure) {
                    Ok(url) => assert_ne!(loaded, &MediaId::RemoteUrl(url)),
                    Err(error) => panic!("a literal URL must normalize: {error}"),
                }
            }
            other => panic!("the second command must be the load: {other:?}"),
        }
    }

    fn local(path: &str) -> MediaId {
        match AbsolutePath::new(path.into()) {
            Ok(path) => MediaId::LocalFile(path),
            Err(error) => panic!("a literal absolute path must parse: {error}"),
        }
    }

    /// A podcast episode identity, for the `open_persistence` tests that
    /// exercise `resume_intent_for`'s decision to resume (Task 8: a local
    /// file never does, on a fresh load, so those tests moved here).
    fn episode(guid: &str) -> MediaId {
        let feed = match FeedId::new("0123456789abcdef0123456789abcdef".to_string()) {
            Ok(feed) => feed,
            Err(error) => panic!("a literal feed ID must parse: {error}"),
        };
        let episode = match EpisodeKey::resolve(Some(guid), None, None) {
            Ok(episode) => episode,
            Err(error) => panic!("a literal key must resolve: {error}"),
        };
        MediaId::PodcastEpisode { feed, episode }
    }

    /// The user-visible half of a resume: the listener sees the restored
    /// position the moment the track opens, not after the first progress tick.
    /// `Loaded` is the only event that carries it.
    #[test]
    fn a_resumed_position_is_shown_as_soon_as_the_track_opens() {
        let mut mirror = Mirror::default();
        mirror.apply(PlaybackEvent::Loaded {
            session_rev: 3,
            request: LoadRequestId::from_raw(1),
            media: local("/music/sonata.flac"),
            metadata: MediaMetadata {
                title: None,
                artist: None,
                album: None,
                year: None,
                duration: Some(Duration::from_secs(300)),
                duration_provenance: PositionProvenance::Established,
                front_cover: None,
            },
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::from_secs(93),
            disposition: StartDisposition::Resumed,
        });

        assert_eq!(mirror.position, Duration::from_secs(93));
        assert_eq!(mirror.quality, PositionQuality::Exact);
        assert!(
            status_line(&mirror).contains("00:01:33"),
            "the resumed position is on the first line drawn, not 00:00:00: {}",
            status_line(&mirror)
        );
    }

    /// §11: the restored level has to be in force from the first buffer, which
    /// is only true if the volume command precedes the load. Pinned on the
    /// sequence itself — two commands the engine applied in order leave no
    /// trace of that order in the events it emits, so nothing downstream can
    /// check this.
    #[test]
    fn the_resume_sequence_restores_volume_before_it_loads() {
        let candidate = ResumeCandidate {
            position: Duration::from_secs(93),
            completed: false,
        };
        let commands = resume_commands(
            local("/music/sonata.flac"),
            SourceLocation::LocalPath("/music/sonata.flac".into()),
            Some(ResumeIntent::Candidate(candidate)),
            Volume::new(0.25),
            LoadRequestId::from_raw(1),
        );

        match &commands {
            [
                PlaybackCommand::SetVolume(volume),
                PlaybackCommand::Load { resume, .. },
                PlaybackCommand::PlayLoaded { .. },
            ] => {
                assert_eq!(*volume, Volume::new(0.25), "the stored gain, unchanged");
                assert_eq!(
                    *resume,
                    ResumeIntent::Candidate(candidate),
                    "the persisted candidate, carried onward for the worker to resolve"
                );
            }
            other => panic!("volume must be issued before the load: {other:?}"),
        }
    }

    /// A store in a tempdir. Nothing in these tests reaches `$HOME`:
    /// `platform_path` is called by `run` and by nothing else, which is exactly
    /// what hoisting it out of `open_persistence` buys.
    fn store_at(path: &std::path::Path) -> (StateStore, Arc<dyn Clock>) {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        (
            StateStore::new(path.to_path_buf(), Arc::clone(&clock)),
            clock,
        )
    }

    #[test]
    fn a_stored_entry_becomes_the_resume_candidate_and_the_restored_volume() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = episode("ep-1");
        let mut stored = PersistedState::default();
        stored.set_volume(Volume::new(0.25));
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(93),
                updated_at: clock.sample().wall,
            },
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(store, &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::Candidate(ResumeCandidate {
                position: Duration::from_secs(93),
                completed: false,
            })),
            "the entry the file held, unresolved — only the worker's probe has a duration"
        );
        assert_eq!(persistence.volume, Volume::new(0.25));
        assert!(persistence.persisting);
    }

    /// §4.3 (Task 6 fix round 1): an entry carrying both an `estimated`
    /// location and its established fallback resolves to
    /// `ResumeIntent::EstimatedCandidate`, preferring the estimate and
    /// keeping the established position in reserve. Ablation: a
    /// `resume_intent_for` that never calls `restart_preference` (the
    /// pre-fix-round state) makes this fail — `persistence.resume` would
    /// read `Some(ResumeIntent::Candidate(..))` instead.
    #[test]
    fn a_stored_estimate_and_its_established_fallback_resolve_to_an_estimated_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = episode("ep-2");
        let mut stored = PersistedState::default();
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(40),
                updated_at: clock.sample().wall,
            },
            false,
        );
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(store, &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::EstimatedCandidate {
                target: Duration::from_secs(97),
                established: Some(Duration::from_secs(40)),
            })
        );
    }

    /// R8: an entry that only ever carried an estimate must resolve with
    /// `established: None`, never a fabricated zero. Ablation: the same as
    /// above, plus — a `resume_intent_for` that reads an absent `position`
    /// as `Duration::ZERO` (the exact loss `resume_candidate` was written to
    /// avoid on the established side) would make this fail on the
    /// `established` field alone while the sibling test above still passes.
    #[test]
    fn a_stored_estimate_with_no_established_position_resolves_with_no_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = episode("ep-3");
        let mut stored = PersistedState::default();
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(store, &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::EstimatedCandidate {
                target: Duration::from_secs(97),
                established: None,
            })
        );
    }

    /// A completed entry must ignore a stray `estimated` field entirely: D1
    /// still resumes it through the ordinary `Candidate`/`CompletedReplay`
    /// path, not `EstimatedCandidate`. `resume_intent_for`'s own doc says a
    /// completed entry never reaches `restart_preference` — this is the
    /// test that would fail if that guard were removed (a `completed`
    /// entry's `resume` would read `EstimatedCandidate` instead of
    /// `Candidate`).
    #[test]
    fn a_completed_entry_ignores_a_stray_estimate_and_stays_an_ordinary_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = episode("ep-4");
        let mut stored = PersistedState::default();
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            true,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(store, &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::Candidate(ResumeCandidate {
                position: Duration::ZERO,
                completed: true,
            }))
        );
    }

    /// The sink selection is the whole feature in one line: swap the store for
    /// `DisabledSink` and persistence silently never writes again. Nothing else
    /// would notice — every other writer test drives a sink of its own — so this
    /// is the one test that follows a submitted snapshot all the way to the
    /// bytes on disk, through `impl StateSink for StateStore` and through the
    /// `Written` arm of the flush report.
    #[test]
    fn a_submitted_snapshot_reaches_the_state_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let (store, clock) = store_at(&path);
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(93),
                updated_at: clock.sample().wall,
            },
            false,
        );
        store.write(&stored).unwrap();

        let mut persistence = open_persistence(store, &media, &clock);
        assert!(persistence.persisting);

        // The listener got another minute in, and the volume moved with them.
        let mut advanced = PersistedState::default();
        advanced.set_volume(Volume::new(0.5));
        advanced.set_current_media(media.clone());
        advanced.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(150),
                updated_at: clock.sample().wall,
            },
            false,
        );
        persistence.writer.submit(advanced, Urgency::Forced);

        let outcome = persistence.writer.shutdown();
        assert!(
            matches!(outcome, ShutdownOutcome::Written),
            "the store must acknowledge the final write: {outcome:?}"
        );
        assert!(matches!(
            classify_flush(outcome, persistence.persisting),
            FlushReport::Written
        ));

        let bytes = std::fs::read(&path).unwrap();
        let written: PersistedState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            written.entry_for(&media).unwrap().position,
            Some(Duration::from_secs(150)),
            "the file must hold the snapshot that was submitted, not the one it started with"
        );
        assert_eq!(written.volume(), Volume::new(0.5));
        assert_eq!(written.current_media(), Some(&media));
    }

    /// D3: a file this build cannot read is preserved in place and writing is
    /// off for the session. The sink the disable selects has to write nowhere,
    /// or the preservation is a claim rather than a fact.
    #[test]
    fn a_state_file_from_a_newer_build_disables_writing_and_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let newer = br#"{"schema_version":99,"current_media":null,"volume":0.5,"checkpoints":{}}"#;
        std::fs::write(&path, newer).unwrap();
        let (store, clock) = store_at(&path);
        let media = local("/music/sonata.flac");

        let mut persistence = open_persistence(store, &media, &clock);

        assert!(!persistence.persisting);
        assert_eq!(
            persistence.resume, None,
            "nothing is restored from a file this build cannot read"
        );
        assert_eq!(persistence.volume, Volume::FULL);

        persistence
            .writer
            .submit(PersistedState::default(), Urgency::Forced);
        let outcome = persistence.writer.shutdown();
        assert!(
            matches!(
                classify_flush(outcome, persistence.persisting),
                FlushReport::Disabled
            ),
            "a session that wrote nothing must not be reported as having written"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            newer,
            "the preserved file must come out byte for byte as it went in"
        );
    }

    #[test]
    fn a_disabled_session_never_reports_a_written_checkpoint() {
        // The sink answers `Ok` for a write it deliberately never performed, so
        // the outcome on its own cannot tell the two apart.
        assert!(matches!(
            classify_flush(ShutdownOutcome::Written, false),
            FlushReport::Disabled
        ));
        assert!(matches!(
            classify_flush(ShutdownOutcome::Written, true),
            FlushReport::Written
        ));
    }

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

    /// An episode title is decoder metadata off the network, so it reaches the
    /// terminal through the same escaping the listings give feed titles.
    #[test]
    fn a_title_carrying_a_terminal_escape_is_rendered_inert() {
        let mirror = Mirror {
            name: Some("Радио-Т\u{1b}[2J\u{202e}1030".to_owned()),
            ..Mirror::default()
        };
        let (name, _) = status_parts(&mirror);
        assert!(
            !name.contains('\u{1b}'),
            "raw ESC reached the row: {name:?}"
        );
        assert!(
            !name.contains('\u{202e}'),
            "raw bidi override reached the row: {name:?}"
        );
        assert!(name.starts_with("Радио-Т"));
    }

    // --------------------------------------------------------- status_line

    fn mirror_with_capabilities(seek: SeekSupport) -> Mirror {
        Mirror {
            capabilities: Some(MediaCapabilities {
                continuity: Continuity::Finite,
                seek,
            }),
            ..Mirror::default()
        }
    }

    #[test]
    fn a_live_source_reads_live_not_as_a_duration() {
        let mut mirror = mirror_with_capabilities(SeekSupport::Unsupported);
        if let Some(capabilities) = mirror.capabilities.as_mut() {
            capabilities.continuity = Continuity::Indefinite;
        }
        let line = status_line(&mirror);
        assert!(line.contains("live"), "{line}");
    }

    #[test]
    fn an_unresolved_seek_capability_is_marked_distinctly_from_unsupported() {
        let unresolved = status_line(&mirror_with_capabilities(SeekSupport::Unknown));
        let unsupported = status_line(&mirror_with_capabilities(SeekSupport::Unsupported));
        assert!(unresolved.contains("seek?"), "{unresolved}");
        assert!(!unresolved.contains("no-seek"), "{unresolved}");
        assert!(unsupported.contains("no-seek"), "{unsupported}");
    }

    #[test]
    fn a_seekable_source_carries_no_seek_note_at_all() {
        let line = status_line(&mirror_with_capabilities(SeekSupport::Native));
        assert!(!line.contains("seek?"), "{line}");
        assert!(!line.contains("no-seek"), "{line}");
    }

    #[test]
    fn buffering_is_shown_only_as_a_detail_of_playing() {
        let mut mirror = Mirror {
            state: PlaybackState::Playing,
            buffering: true,
            ..Mirror::default()
        };
        assert!(
            status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );

        // Never a state of its own: a paused session that happens to be
        // servicing a blocked read (e.g. a seek's own reopen) does not print
        // "buffering" under a label that is not Playing.
        mirror.state = PlaybackState::Paused;
        assert!(
            !status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );
    }

    /// §11: `buffering` must never be derived from `PositionQuality::Degraded`
    /// (Ruling 4) - a degraded quality with `buffering` still false must not
    /// print "buffering" on its own account.
    #[test]
    fn degraded_quality_does_not_imply_buffering() {
        let mirror = Mirror {
            state: PlaybackState::Playing,
            quality: PositionQuality::Degraded,
            buffering: false,
            ..Mirror::default()
        };
        assert!(
            !status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );
    }

    // ----------------------------------------------------- Mirror::apply

    #[test]
    fn capabilities_changed_updates_the_mirror_without_disturbing_position() {
        let mut mirror = Mirror {
            position: Duration::from_secs(42),
            ..Mirror::default()
        };
        mirror.apply(PlaybackEvent::CapabilitiesChanged {
            session_rev: 7,
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
        });
        assert_eq!(mirror.session_rev, 7);
        assert_eq!(
            mirror.capabilities.map(|capabilities| capabilities.seek),
            Some(SeekSupport::Native)
        );
        assert_eq!(
            mirror.position,
            Duration::from_secs(42),
            "unrelated to capability evidence"
        );
    }

    /// Minor (final review): a stale `buffering` from whatever the mirror was
    /// showing before must not survive into a fresh `Loaded` - display-only
    /// and unreachable under one load per run, but a real bug rather than
    /// only a limitation.
    #[test]
    fn a_fresh_load_clears_a_stale_buffering_flag() {
        let mut mirror = Mirror {
            buffering: true,
            ..Mirror::default()
        };
        mirror.apply(PlaybackEvent::Loaded {
            session_rev: 3,
            request: LoadRequestId::from_raw(1),
            media: local("/music/sonata.flac"),
            metadata: MediaMetadata {
                title: None,
                artist: None,
                album: None,
                year: None,
                duration: Some(Duration::from_secs(300)),
                duration_provenance: PositionProvenance::Established,
                front_cover: None,
            },
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        });
        assert!(
            !mirror.buffering,
            "a fresh load must clear a stale buffering flag"
        );
    }

    // ------------------------------------------------------ apply_progress

    fn progress_at(position: Duration) -> Progress {
        Progress {
            session_rev: 0,
            media: None,
            position,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
            load: None,
        }
    }

    #[test]
    fn progress_arriving_during_a_burst_does_not_yank_the_display_back() {
        // The worker has not been asked to move yet, so its progress reports
        // where playback still is. Copying that over the optimistic target
        // would undo the jump on the very next tick - roughly 100ms after the
        // press, which reads as the key not having worked.
        let mut mirror = Mirror {
            position: Duration::from_secs(60),
            provenance: PositionProvenance::Estimated,
            ..Mirror::default()
        };
        apply_progress(&mut mirror, &progress_at(Duration::from_secs(100)), true);
        assert_eq!(mirror.position, Duration::from_secs(60));
        assert_eq!(mirror.provenance, PositionProvenance::Estimated);
    }

    #[test]
    fn buffering_still_reaches_the_display_during_a_burst() {
        // Unrelated to the target: suppressing it would blank the buffering
        // note for as long as the listener kept scrubbing.
        let mut mirror = Mirror::default();
        let progress = Progress {
            buffering: true,
            ..progress_at(Duration::from_secs(100))
        };
        apply_progress(&mut mirror, &progress, true);
        assert!(mirror.buffering);
    }

    #[test]
    fn the_landing_corrects_the_display_once_the_burst_closes() {
        let mut mirror = Mirror {
            position: Duration::from_secs(60),
            provenance: PositionProvenance::Estimated,
            ..Mirror::default()
        };
        apply_progress(&mut mirror, &progress_at(Duration::from_secs(58)), false);
        assert_eq!(mirror.position, Duration::from_secs(58));
        assert_eq!(mirror.provenance, PositionProvenance::Established);
    }
}
