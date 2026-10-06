//! Routes a decoded key command to the engine, coalescing a burst of
//! arrow-key presses into the single seek they mean rather than one per
//! press. [`KeyRouter`] is the entry point; [`SeekBurst`] is the
//! accumulator it owns.

use std::time::{Duration, Instant};

use crate::playback::command::{Admission, PlaybackCommand};
use crate::playback::engine::EngineHandle;
use crate::playback::event::PlaybackEvent;

/// Sends one decoded key command through `EngineHandle::submit`, which
/// applies each command's out-of-band rule itself (M9.4). `Shutdown` never
/// reaches here: a front end ends its own loop on a quit and has nothing left
/// to route.
///
/// `SeekBy` is the one command this does not handle: an arrow press
/// accumulates into a [`SeekBurst`] rather than reaching the engine on its
/// own, so it is routed by [`KeyRouter::route`] before it ever gets here.
fn route_command(engine: &EngineHandle, command: PlaybackCommand) {
    match command {
        // Loop control, decided by the front end itself before this is ever
        // called - nothing to route.
        PlaybackCommand::Shutdown => {}
        // Never reaches here - `KeyRouter::route` intercepts it into the
        // burst. Submitting it raw would resolve the target against a mirror
        // that cannot have moved since the last press, which is the defect
        // the burst exists to fix.
        PlaybackCommand::SeekBy(_) => {
            debug_assert!(false, "SeekBy must be routed through the seek burst");
        }
        other => {
            report_admission(engine.submit(other));
        }
    }
}

/// Owns the [`SeekBurst`] across loop passes and routes every decoded key
/// command through it.
///
/// `pub`, alongside the rest of this crate's engine-facing surface
/// (`EngineHandle`, `PlaybackCommand`), so a test can drive the exact routing
/// a keypress takes with no tty and no crossterm event in the loop at all —
/// a front end's key loop cannot be driven headlessly, since
/// `crossterm::event::read()` needs a real terminal. This is the only entry
/// point production uses, so a test driving it cannot be exercising a path
/// the application has stopped taking.
#[derive(Debug, Default)]
pub struct KeyRouter {
    burst: SeekBurst,
    /// The target submitted and not yet accounted for by the worker.
    ///
    /// Submitting is not arriving: the worker goes on reporting the pre-seek
    /// position until the reopen and the buffering are done, so the display
    /// has to keep showing the target across that window too - and a further
    /// press has to accumulate from it rather than from the mirror.
    submitted: Option<Duration>,
    /// A target the worker stored rather than ran (a seek while stopped or
    /// recovering, M10 §7). Held until a landing resolves it, so a later
    /// burst accumulates from it, not from a mirror that progress has put
    /// back at the heard position.
    stored: Option<Duration>,
}

impl KeyRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `SeekBy` arm of [`Self::route`], which needs no engine. Split out
    /// so a test can drive the accumulation production performs rather than a
    /// reimplementation of it.
    fn press(
        &mut self,
        position: Duration,
        step: i64,
        now: Instant,
        duration: Option<Duration>,
    ) -> Duration {
        // Seeded from the target already on display rather than from
        // `position` whenever one is standing. `position` comes from the
        // mirror, and the mirror cannot have moved since the last press: it
        // only advances on progress, and the worker publishes none while it is
        // inside a seek. Re-reading it is what collapsed a burst of presses
        // onto a single step.
        let base = self.displayed_target().unwrap_or(position);
        self.burst.press(base, step, now, duration)
    }

    /// The target the display is currently showing, if it is showing one
    /// rather than the worker's own position.
    fn displayed_target(&self) -> Option<Duration> {
        self.burst.target().or(self.submitted).or(self.stored)
    }

    /// The target to submit, marking the wait for its landing as begun.
    fn take_due(&mut self, now: Instant) -> Option<Duration> {
        let target = self.burst.due(now)?;
        self.submitted = Some(target);
        Some(target)
    }

    /// Lets the router see each drained event, so it can tell when the seek
    /// it is waiting on has settled: landed, been refused, been overtaken, or
    /// been stored to land later.
    ///
    /// A superseding load — `Loaded`, `LoadCancelled` or `Failed` — cancels
    /// any unsubmitted burst outright rather than merely releasing the
    /// displayed target: a burst accumulated against the track that just
    /// left is not a seek this session should ever go on to submit once a
    /// different (or no) track is current.
    pub fn observe(&mut self, event: &PlaybackEvent) {
        match event {
            // Stored, not run: hold the worker's own (clamped) target.
            PlaybackEvent::SeekTargetStored { target, .. } => {
                self.submitted = None;
                self.stored = Some(*target);
            }
            PlaybackEvent::SeekCompleted { .. }
            | PlaybackEvent::SeekRejected { .. }
            | PlaybackEvent::SeekCancelled { .. }
            | PlaybackEvent::RestartEstablished { .. }
            | PlaybackEvent::EndOfTrack { .. } => self.release(),
            PlaybackEvent::Loaded { .. }
            | PlaybackEvent::LoadCancelled { .. }
            | PlaybackEvent::Failed { .. } => self.cancel(),
            _ => {}
        }
    }

    /// Drops an accumulated seek and the display hold together, for a
    /// command or a load outcome that supersedes both.
    pub fn cancel(&mut self) {
        self.burst.cancel();
        self.release();
    }

    /// Hands the display back to the worker's own position.
    fn release(&mut self) {
        self.submitted = None;
        self.stored = None;
    }

    /// Drops an unsubmitted burst and the wait for a submitted one, keeping a
    /// stored target: the worker keeps its `pending` across a stop, and a
    /// restart replaces it with a `SeekTargetStored` of its own.
    fn drop_burst(&mut self) {
        self.burst.cancel();
        self.submitted = None;
    }

    /// Routes one command. Returns the position the display should adopt
    /// immediately when an arrow press accumulated into the burst, and `None`
    /// for every command that leaves the displayed position alone.
    ///
    /// `position` and `duration` are the two `Mirror` fields this routing
    /// reads, taken separately so `Mirror` itself can stay private.
    pub fn route(
        &mut self,
        engine: &EngineHandle,
        position: Duration,
        duration: Option<Duration>,
        now: Instant,
        command: PlaybackCommand,
    ) -> Option<Duration> {
        match command {
            PlaybackCommand::SeekBy(step) => {
                return Some(self.press(position, step, now, duration));
            }
            // An absolute move, a stop, or the end of the run supersedes an
            // accumulated relative seek outright. Submitting the burst first
            // would spend a fetch on a target the very next command discards.
            PlaybackCommand::Restart | PlaybackCommand::Stop | PlaybackCommand::Shutdown => {
                self.drop_burst();
            }
            // Volume and pause/play move nothing, so they coexist with an open
            // burst: routing them must not cost the listener their scrub.
            _ => {}
        }
        route_command(engine, command);
        None
    }

    /// Submits the accumulated seek once the quiet window has passed. This is
    /// the only place an arrow press reaches the engine.
    pub fn flush(&mut self, engine: &EngineHandle, now: Instant) {
        if let Some(target) = self.take_due(now) {
            // A seek the queue refuses will never report a landing, so the
            // hold has to end here rather than wait for an event that is not
            // coming.
            if report_admission(engine.submit(PlaybackCommand::SeekTo(target)))
                != Admission::Accepted
            {
                self.release();
            }
        }
    }

    /// Whether the display is currently showing an optimistic target rather
    /// than the worker's own position.
    pub fn is_seeking(&self) -> bool {
        self.burst.is_open() || self.submitted.is_some() || self.stored.is_some()
    }

    pub(crate) fn poll_budget(&self, now: Instant, cap: Duration) -> Duration {
        self.burst.poll_budget(now, cap)
    }
}

/// The absolute target an arrow-key seek asks for. `SeekTo` takes a
/// `Duration`, not a delta, so this is the same clamp-at-zero arithmetic
/// `engine.rs`'s own `SeekBy` dispatch performs, computed here instead
/// against the mirror's position now that the CLI resolves the target rather
/// than handing the worker a signed step to resolve against `self.position`.
///
/// `duration` bounds the forward direction when it is known. `None` leaves
/// it unbounded on purpose: `clamp_target` in the engine bounds a target
/// against a duration this side has not learned yet, and inventing a
/// ceiling here would cap a seek the engine could have satisfied.
fn seek_target(position: Duration, delta: i64, duration: Option<Duration>) -> Duration {
    let step = Duration::from_secs(delta.unsigned_abs());
    if delta >= 0 {
        let target = position.saturating_add(step);
        match duration {
            Some(duration) => target.min(duration),
            None => target,
        }
    } else {
        position.saturating_sub(step)
    }
}

/// How long a burst of arrow presses stays open, waiting for the next one.
///
/// Key repeat delivers a held arrow roughly every 30ms, well inside this, so
/// holding the key scrubs continuously and commits one window after release.
const SEEK_COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// A run of arrow-key presses collapsed into a single seek.
///
/// Each press resolves its target in this thread against the mirror, and the
/// mirror only advances when the worker publishes progress - which it does
/// not do while it is inside a seek, reopening a range request and buffering.
/// Resolving each press of a burst independently against that frozen position
/// therefore produced N identical `SeekTo` commands: the listener moved one
/// step however many times they pressed, and every one of those commands
/// called `source_interrupt.retire()` on the fetch its predecessor had just
/// started.
///
/// Accumulating onto the previous target instead of re-reading the mirror is
/// what makes presses compose, and holding them for a quiet window is what
/// spends one fetch on the burst rather than one per press.
#[derive(Debug, Default)]
pub struct SeekBurst {
    open: Option<OpenBurst>,
}

#[derive(Debug)]
struct OpenBurst {
    /// The absolute target accumulated so far. Stored rather than recomputed
    /// from a base and a delta so that what the display was told and what is
    /// eventually submitted cannot drift apart.
    target: Duration,
    deadline: Instant,
}

impl SeekBurst {
    /// Accumulates one arrow-key step onto `base`, returning the target the
    /// display should jump to at once. Choosing `base` is
    /// [`KeyRouter::press`]'s job - the single place that rule lives.
    fn press(
        &mut self,
        base: Duration,
        step: i64,
        now: Instant,
        duration: Option<Duration>,
    ) -> Duration {
        let target = seek_target(base, step, duration);
        self.open = Some(OpenBurst {
            target,
            deadline: now + SEEK_COALESCE_WINDOW,
        });
        target
    }

    /// The target to submit, once the quiet window has passed with no further
    /// press. Closes the burst, so a target is handed out exactly once.
    fn due(&mut self, now: Instant) -> Option<Duration> {
        let open = self.open.as_ref()?;
        if now < open.deadline {
            return None;
        }
        self.open.take().map(|open| open.target)
    }

    fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The target accumulated so far, if a burst is open.
    fn target(&self) -> Option<Duration> {
        self.open.as_ref().map(|open| open.target)
    }

    /// Drops the burst without submitting anything - for a command that
    /// supersedes it outright rather than merely coexisting with it.
    fn cancel(&mut self) {
        self.open = None;
    }

    /// How long the key poll may block. The loop polls in `cap`-sized blocks;
    /// left uncapped, a window expiring just after a block began would not be
    /// noticed until a full block later.
    fn poll_budget(&self, now: Instant, cap: Duration) -> Duration {
        match &self.open {
            Some(open) => open.deadline.saturating_duration_since(now).min(cap),
            None => cap,
        }
    }
}

/// §8: queue saturation must be visible, never silently dropped. `Gone`
/// means the worker has already shut down - nothing to warn about, since the
/// run is ending anyway.
fn report_admission(admission: Admission) -> Admission {
    if admission == Admission::Busy {
        tracing::warn!("command queue is busy; the key press had no effect");
    }
    admission
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use crate::media::id::{AbsolutePath, MediaId};
    use crate::media::metadata::MediaMetadata;
    use crate::playback::command::LoadRequestId;
    use crate::playback::event::StartDisposition;
    use crate::playback::provenance::PositionProvenance;
    use crate::playback::volume::Volume;

    /// The step this file's own key mapping uses for an arrow press. A local
    /// copy rather than an import: `SEEK_STEP_SECS` is a key-mapping detail
    /// that belongs to `app.rs`, and these tests only need some fixed step,
    /// not that particular one.
    const SEEK_STEP_SECS: i64 = 10;

    /// A burst opened at `position`, with `count` presses of `step`, all
    /// arriving at the same instant — the shape a held or hammered arrow key
    /// produces, and the one where the mirror cannot have advanced between
    /// presses.
    ///
    /// Driven through `KeyRouter`, the type the key loop actually presses
    /// into, rather than through `SeekBurst` underneath it - which on its own
    /// does not decide what a press accumulates from.
    fn burst_of(position: Duration, step: i64, count: usize) -> (KeyRouter, Instant) {
        burst_of_within(position, step, count, None)
    }

    fn burst_of_within(
        position: Duration,
        step: i64,
        count: usize,
        duration: Option<Duration>,
    ) -> (KeyRouter, Instant) {
        let now = Instant::now();
        let mut router = KeyRouter::new();
        for _ in 0..count {
            router.press(position, step, now, duration);
        }
        (router, now)
    }

    #[test]
    fn four_quick_left_presses_accumulate_into_one_forty_second_seek() {
        // The bug this exists for: every press resolves against the mirror,
        // and the mirror cannot advance while the worker is inside a seek
        // publishing no progress. Resolving each press independently against
        // that frozen position collapses all four onto the same 10s target,
        // so the listener moves 10s and pays four reopens.
        let (mut router, now) = burst_of(Duration::from_secs(100), -SEEK_STEP_SECS, 4);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn a_burst_submits_nothing_until_the_quiet_window_expires() {
        let (mut router, now) = burst_of(Duration::from_secs(100), -SEEK_STEP_SECS, 1);
        assert_eq!(router.take_due(now), None);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW - Duration::from_millis(1)),
            None
        );
        assert!(router.take_due(now + SEEK_COALESCE_WINDOW).is_some());
    }

    #[test]
    fn a_second_press_extends_the_window_rather_than_letting_the_first_expire() {
        // Without the extension, holding the key would fire a seek every
        // window - the thrash this is meant to collapse, merely slower.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None);
        let later = now + SEEK_COALESCE_WINDOW - Duration::from_millis(50);
        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, later, None);
        assert_eq!(router.take_due(now + SEEK_COALESCE_WINDOW), None);
        assert_eq!(
            router.take_due(later + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(80))
        );
    }

    #[test]
    fn a_burst_is_closed_once_it_comes_due_and_does_not_submit_twice() {
        let (mut router, now) = burst_of(Duration::from_secs(100), -SEEK_STEP_SECS, 1);
        let due = now + SEEK_COALESCE_WINDOW;
        assert!(router.take_due(due).is_some());
        assert_eq!(router.take_due(due), None, "the same burst came due twice");
    }

    #[test]
    fn a_backward_burst_clamps_at_zero_rather_than_wrapping() {
        let (mut router, now) = burst_of(Duration::from_secs(15), -SEEK_STEP_SECS, 4);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_forward_burst_clamps_at_a_known_duration() {
        let (mut router, now) = burst_of_within(
            Duration::from_secs(80),
            SEEK_STEP_SECS,
            4,
            Some(Duration::from_secs(100)),
        );
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(100))
        );
    }

    #[test]
    fn an_unknown_duration_leaves_a_forward_burst_unclamped_for_the_engine_to_bound() {
        // `clamp_target` in the engine is what bounds a target against a
        // duration this side does not know; inventing a ceiling here would
        // silently cap a seek the engine could have satisfied.
        let (mut router, now) = burst_of(Duration::from_secs(80), SEEK_STEP_SECS, 4);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn press_returns_the_optimistic_target_for_the_display_to_show_at_once() {
        let now = Instant::now();
        let mut router = KeyRouter::new();
        assert_eq!(
            router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None),
            Duration::from_secs(90)
        );
        assert_eq!(
            router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None),
            Duration::from_secs(80),
            "the second press resolved against the mirror again instead of \
             against the target the first press already accumulated"
        );
    }

    #[test]
    fn the_display_holds_the_target_from_the_press_until_the_seek_lands() {
        // Submitting is not arriving. Between the flush and the landing the
        // worker goes on reporting where playback still is - it has a range
        // request to reopen and bytes to buffer first - so releasing the hold
        // at the flush would snap the display back to the old position for
        // the whole of that wait and then jump a second time. The flicker the
        // optimistic jump exists to avoid, merely moved later.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        assert!(!router.is_seeking());

        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None);
        assert!(router.is_seeking(), "the press did not open the hold");

        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(90))
        );
        assert!(
            router.is_seeking(),
            "the hold ended at the flush rather than at the landing"
        );

        router.observe(&PlaybackEvent::SeekCompleted {
            session_rev: 0,
            requested: Duration::from_secs(90),
            actual: Duration::from_secs(90),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        });
        assert!(!router.is_seeking(), "the landing did not release the hold");
    }

    #[test]
    fn a_refused_seek_releases_the_hold_too() {
        // Otherwise the display freezes on a target it will never reach.
        // Symphonia refuses a seek past the last frame outright, so this is a
        // reachable case, not a defensive one: press the arrow enough times
        // near the end of a track and the seek comes back rejected.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        assert!(router.take_due(now + SEEK_COALESCE_WINDOW).is_some());
        router.observe(&PlaybackEvent::SeekRejected {
            session_rev: 0,
            reason: "cannot seek to 110s".to_string(),
        });
        assert!(!router.is_seeking());
    }

    #[test]
    fn an_unrelated_event_leaves_the_hold_alone() {
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None);
        assert!(router.take_due(now + SEEK_COALESCE_WINDOW).is_some());
        router.observe(&PlaybackEvent::VolumeChanged {
            session_rev: 0,
            volume: Volume::default(),
        });
        assert!(
            router.is_seeking(),
            "a volume change released a hold that only a seek outcome should"
        );
    }

    #[test]
    fn a_press_during_the_wait_for_a_landing_reopens_the_burst() {
        // Pressing again while the previous seek is still in flight must
        // accumulate from what the display is showing, not from the mirror -
        // which is exactly the position the in-flight seek is moving away
        // from.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None);
        let submitted = now + SEEK_COALESCE_WINDOW;
        assert_eq!(router.take_due(submitted), Some(Duration::from_secs(90)));
        assert_eq!(
            router.press(Duration::from_secs(100), -SEEK_STEP_SECS, submitted, None),
            Duration::from_secs(80),
            "the new burst reseeded from the stale mirror instead of from the \
             target the display is already showing"
        );
    }

    #[test]
    fn a_cancelled_burst_submits_nothing() {
        let (mut router, now) = burst_of(Duration::from_secs(100), -SEEK_STEP_SECS, 3);
        router.cancel();
        assert!(!router.is_seeking());
        assert_eq!(router.take_due(now + SEEK_COALESCE_WINDOW), None);
    }

    /// The behavior change this move carries: a finished, failed or
    /// cancelled load must discard an unsubmitted burst outright, not merely
    /// release the displayed target - a burst accumulated for one track must
    /// never survive into whatever loads next.
    #[test]
    fn loaded_and_failed_events_cancel_unsubmitted_bursts() {
        fn local(path: &str) -> MediaId {
            match AbsolutePath::new(path.into()) {
                Ok(path) => MediaId::LocalFile(path),
                Err(error) => panic!("a literal absolute path must parse: {error}"),
            }
        }

        let loaded = PlaybackEvent::Loaded {
            session_rev: 1,
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
        };
        let load_cancelled = PlaybackEvent::LoadCancelled {
            session_rev: 1,
            request: LoadRequestId::from_raw(1),
        };
        let failed = PlaybackEvent::Failed {
            session_rev: 1,
            message: "device gone".to_string(),
            cause: None,
            request: Some(LoadRequestId::from_raw(1)),
        };

        for event in [loaded, load_cancelled, failed] {
            let now = Instant::now();
            let mut router = KeyRouter::new();
            router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
            assert!(router.is_seeking(), "the press did not open the hold");

            router.observe(&event);
            assert!(
                !router.is_seeking(),
                "a superseding load must cancel an unsubmitted burst: {event:?}"
            );

            let later = now + SEEK_COALESCE_WINDOW;
            assert_eq!(
                router.take_due(later),
                None,
                "a cancelled burst must never come due: {event:?}"
            );
        }
    }

    #[test]
    fn an_open_burst_shortens_the_key_poll_to_its_own_deadline() {
        // The loop polls for keys in 100ms blocks; left alone, a 250ms window
        // would be noticed up to a full block late.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        let cap = Duration::from_millis(100);
        assert_eq!(
            router.poll_budget(now, cap),
            cap,
            "an idle burst caps nothing"
        );
        router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None);
        assert_eq!(
            router.poll_budget(now, cap),
            cap,
            "250ms away, the cap still wins"
        );
        let near = now + SEEK_COALESCE_WINDOW - Duration::from_millis(20);
        assert_eq!(router.poll_budget(near, cap), Duration::from_millis(20));
        assert_eq!(
            router.poll_budget(now + SEEK_COALESCE_WINDOW, cap),
            Duration::ZERO
        );
    }

    #[test]
    fn a_stored_target_holds_the_display_and_seeds_the_next_burst() {
        // M10 §7: a seek while stopped or recovering is stored, not run. The
        // progress that follows reports the heard position, which the next
        // burst must not start from.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        assert_eq!(
            router.take_due(now + SEEK_COALESCE_WINDOW),
            Some(Duration::from_secs(110))
        );
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        assert!(
            router.is_seeking(),
            "a stored target must keep the display on it"
        );
        let later = now + Duration::from_secs(5);
        assert_eq!(
            router.press(Duration::from_secs(100), SEEK_STEP_SECS, later, None),
            Duration::from_secs(120),
            "the burst started from the mirror instead of the stored target"
        );
    }

    #[test]
    fn the_stored_target_is_the_workers_clamped_one() {
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        router.take_due(now + SEEK_COALESCE_WINDOW);
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(105),
        });
        assert_eq!(
            router.press(Duration::from_secs(100), -SEEK_STEP_SECS, now, None),
            Duration::from_secs(95)
        );
    }

    #[test]
    fn the_landing_releases_a_stored_target() {
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        router.observe(&PlaybackEvent::SeekCompleted {
            session_rev: 0,
            requested: Duration::from_secs(110),
            actual: Duration::from_secs(110),
            refinement_truncated: false,
            provenance: PositionProvenance::Established,
        });
        assert!(!router.is_seeking());
    }

    #[test]
    fn a_restart_landing_releases_a_stored_target() {
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::ZERO,
        });
        router.observe(&PlaybackEvent::RestartEstablished {
            session_rev: 0,
            position: Duration::ZERO,
            provenance: PositionProvenance::Established,
        });
        assert!(!router.is_seeking());
    }

    #[test]
    fn a_stop_drops_the_burst_but_keeps_the_stored_target() {
        // The worker keeps `pending` across a stop, so the next burst must
        // still accumulate on it.
        let now = Instant::now();
        let mut router = KeyRouter::new();
        router.observe(&PlaybackEvent::SeekTargetStored {
            session_rev: 0,
            target: Duration::from_secs(110),
        });
        router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None);
        router.drop_burst();
        assert_eq!(router.take_due(now + SEEK_COALESCE_WINDOW), None);
        assert!(router.is_seeking());
        assert_eq!(
            router.press(Duration::from_secs(100), SEEK_STEP_SECS, now, None),
            Duration::from_secs(120)
        );
    }
}
