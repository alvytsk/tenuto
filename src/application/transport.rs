//! The transport decision table (design doc §4, M8 §7): given a transport
//! key, the navigation and viewed playlists and the playback phase, what
//! should happen. Pure and stateless -
//! no engine call, no clock read - so the runtime that drives the engine
//! can be tested against this table without a real device.

use std::time::Duration;

use crate::playlist::Playlist;
use crate::queue::{Direction, QueueEntryId};

pub const PLAY_BEFORE_SEEK: &str = "Play a track before seeking";
pub const QUEUE_EMPTY: &str = "Queue is empty";
pub const TRACK_ENDED: &str = "Track ended; press play to replay";
pub const STILL_LOADING: &str = "Still loading";
pub const LIVE_NO_SEEK: &str = "live stream: seeking is unavailable";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackPhase {
    Unloaded,
    Loading,
    LoadFailed,
    Playing,
    /// A live source between a disconnect and the reconnect that ends it
    /// (M7 §7). Controlled exactly like `Playing`, and named apart from it
    /// only so a front end can say so.
    Reconnecting,
    Paused,
    Stopped,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportInput {
    Space,
    Play,
    Enter,
    Home,
    SeekBy(i64),
    SeekTo(Duration),
    Previous,
    Next,
}

pub struct TransportSituation<'a> {
    /// `playing` — except during `Loading`, the owner of `last_requested`
    /// while that entry is still queued (M8 P5). The caller decides.
    pub navigation: &'a Playlist,
    pub viewed: &'a Playlist,
    /// The selected row of `viewed`. Only Enter reads it.
    pub selected: Option<QueueEntryId>,
    pub phase: PlaybackPhase,
    /// `last_requested`, already validated by the caller through the owner
    /// lookup across *all* playlists: neither playlist here could do it.
    pub retry: Option<QueueEntryId>,
    /// Whether what the engine holds is indefinite — a station, with no
    /// timeline to move around in.
    pub live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportDecision {
    Load(QueueEntryId),
    TogglePause,
    Play,
    SeekBy(i64),
    SeekTo(Duration),
    Restart,
    Notice(&'static str),
    Nothing,
}

/// The selected row of the *viewed* playlist while it is still there, else
/// that playlist's first row. Enter is the only command that reads it
/// (M8 §7); a row removed from under the selection falls back rather than
/// resolving to a `Load` that later queue registration would reject.
fn selection(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    let queue = situation.viewed.queue();
    situation
        .selected
        .filter(|id| queue.get(*id).is_some())
        .or_else(|| queue.first())
}

/// Where Space and `p` start when nothing is held: the navigation playlist's
/// cursor, else its first entry in playback order. Never the viewed tab.
fn start(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    let playlist = situation.navigation;
    playlist
        .queue()
        .active()
        .or_else(|| playlist.first_in_order())
}

/// A `Load` of `id`, or the empty-queue notice if there was nothing to load.
/// Only Enter over an empty viewed playlist actually reaches the notice:
/// every other call site is reached once the navigation playlist is known
/// nonempty, so `start` always returns `Some` there.
fn load_or_notice(id: Option<QueueEntryId>) -> TransportDecision {
    id.map_or(
        TransportDecision::Notice(QUEUE_EMPTY),
        TransportDecision::Load,
    )
}

/// The entry Previous/Next steps from: the retry while loading, else the
/// navigation playlist's cursor. With no anchor there is nothing to step
/// away from, so the key is a no-op rather than a jump to whatever row
/// happens to be selected — which may not even be in this playlist.
fn navigate(situation: &TransportSituation<'_>, direction: Direction) -> TransportDecision {
    let cursor = situation.navigation.queue().active();
    let anchor = match situation.phase {
        PlaybackPhase::Loading => situation.retry.or(cursor),
        _ => cursor,
    };
    anchor
        .and_then(|id| situation.navigation.neighbor(id, direction))
        .map_or(TransportDecision::Nothing, TransportDecision::Load)
}

/// A playing or paused track keeps its engine controls even once its queue
/// has been emptied out from under it: Space still toggles pause, `p` is
/// still idempotent play, and a seek in flight still lands. Only Enter,
/// which would otherwise pick a row that no longer exists, reports the
/// queue as empty.
fn decide_engine_with_empty_queue(input: TransportInput) -> TransportDecision {
    match input {
        TransportInput::Space => TransportDecision::TogglePause,
        TransportInput::Play => TransportDecision::Play,
        TransportInput::Home => TransportDecision::Restart,
        TransportInput::SeekBy(n) => TransportDecision::SeekBy(n),
        TransportInput::SeekTo(d) => TransportDecision::SeekTo(d),
        TransportInput::Previous | TransportInput::Next => TransportDecision::Nothing,
        TransportInput::Enter => TransportDecision::Notice(QUEUE_EMPTY),
    }
}

/// M7 §3.4: a rejected seek must be harmless, and the cheapest way is not
/// to submit one; no `KeyRouter` burst opens either. Shared by the playlist
/// and the detached tables.
fn live_refuses(input: TransportInput, phase: PlaybackPhase, live: bool) -> bool {
    live && matches!(
        input,
        TransportInput::Home | TransportInput::SeekBy(_) | TransportInput::SeekTo(_)
    ) && !matches!(phase, PlaybackPhase::Unloaded | PlaybackPhase::LoadFailed)
}

/// The transport table for media played on its own (`tenuto play`, M9.3):
/// there is no playlist to start, retry or navigate, so every key goes to the
/// engine as itself. What the engine answers (a warning for Space after the
/// end, say) is the engine's rule, not this table's. The one exception is a
/// seek while loading: no position exists to step from until the engine has
/// decided the resume, so it waits, as the playlist table's does.
pub fn decide_detached(
    input: TransportInput,
    phase: PlaybackPhase,
    live: bool,
) -> TransportDecision {
    if live_refuses(input, phase, live) {
        return TransportDecision::Notice(LIVE_NO_SEEK);
    }
    if phase == PlaybackPhase::Loading
        && matches!(input, TransportInput::SeekBy(_) | TransportInput::SeekTo(_))
    {
        return TransportDecision::Notice(STILL_LOADING);
    }
    match input {
        TransportInput::Space => TransportDecision::TogglePause,
        TransportInput::Play => TransportDecision::Play,
        TransportInput::Home => TransportDecision::Restart,
        TransportInput::SeekBy(step) => TransportDecision::SeekBy(step),
        TransportInput::SeekTo(target) => TransportDecision::SeekTo(target),
        TransportInput::Enter | TransportInput::Previous | TransportInput::Next => {
            TransportDecision::Nothing
        }
    }
}

pub fn decide(input: TransportInput, situation: &TransportSituation<'_>) -> TransportDecision {
    // Before the empty-queue branch: what the engine holds is live whether or
    // not the queue still lists it.
    if live_refuses(input, situation.phase, situation.live) {
        return TransportDecision::Notice(LIVE_NO_SEEK);
    }

    use PlaybackPhase::{
        Ended, LoadFailed, Loading, Paused, Playing, Reconnecting, Stopped, Unloaded,
    };
    use TransportInput::{Enter, Home, Next, Play, Previous, SeekBy, SeekTo, Space};

    // Enter follows the view, in every phase, and its emptiness is the
    // viewed playlist's: an empty playing playlist never blocks it.
    if input == Enter {
        return load_or_notice(selection(situation));
    }
    // A valid retry outranks the empty check: the failed request may live in
    // a playlist that is neither playing nor viewed.
    if situation.phase == LoadFailed
        && matches!(input, Space | Play)
        && let Some(id) = situation.retry
    {
        return TransportDecision::Load(id);
    }
    if situation.navigation.queue().is_empty() {
        return match situation.phase {
            // A playing or paused track was not necessarily fed by this now-empty
            // playlist's current contents, so the engine keeps running it.
            Playing | Reconnecting | Paused => decide_engine_with_empty_queue(input),
            // Stopped playback drops its adoption along with the entry that fed
            // it, so restarting would replay media the listener just removed.
            _ => match input {
                Previous | Next => TransportDecision::Nothing,
                _ => TransportDecision::Notice(QUEUE_EMPTY),
            },
        };
    }

    match (situation.phase, input) {
        // Answered by the early return above; this arm only keeps the match
        // exhaustive over `TransportInput`.
        (_, Enter) => load_or_notice(selection(situation)),
        (_, Previous) => navigate(situation, Direction::Up),
        (_, Next) => navigate(situation, Direction::Down),

        (Unloaded | LoadFailed | Ended, Space | Play) => load_or_notice(start(situation)),
        (Unloaded | LoadFailed, Home | SeekBy(_) | SeekTo(_)) => {
            TransportDecision::Notice(PLAY_BEFORE_SEEK)
        }
        (Ended, Home) => TransportDecision::Restart,
        (Ended, SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(TRACK_ENDED),

        (Loading, Space | Play) => TransportDecision::Nothing,
        (Loading, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(STILL_LOADING),

        (Playing | Reconnecting | Paused | Stopped, Space) => TransportDecision::TogglePause,
        (Playing | Reconnecting | Paused | Stopped, Play) => TransportDecision::Play,
        (Playing | Reconnecting | Paused | Stopped, Home) => TransportDecision::Restart,
        (Playing | Reconnecting | Paused | Stopped, SeekBy(n)) => TransportDecision::SeekBy(n),
        (Playing | Reconnecting | Paused | Stopped, SeekTo(d)) => TransportDecision::SeekTo(d),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_track_forwards_every_key_to_the_engine() {
        use PlaybackPhase::{Ended, Loading, Paused, Playing, Reconnecting, Stopped};
        for phase in [Loading, Playing, Paused, Stopped, Ended, Reconnecting] {
            assert_eq!(
                decide_detached(TransportInput::Space, phase, false),
                TransportDecision::TogglePause
            );
            assert_eq!(
                decide_detached(TransportInput::Play, phase, false),
                TransportDecision::Play
            );
            assert_eq!(
                decide_detached(TransportInput::Home, phase, false),
                TransportDecision::Restart
            );
        }
        for phase in [Playing, Paused, Stopped, Ended, Reconnecting] {
            assert_eq!(
                decide_detached(TransportInput::SeekBy(-10), phase, false),
                TransportDecision::SeekBy(-10)
            );
        }
    }

    /// No position is known until the load lands (the engine decides the
    /// resume), so a seek has nothing to step from: it waits, as the
    /// playlist table's does. Home needs no position and still goes through.
    #[test]
    fn a_detached_seek_waits_for_the_load() {
        for input in [
            TransportInput::SeekBy(10),
            TransportInput::SeekTo(Duration::from_secs(30)),
        ] {
            assert_eq!(
                decide_detached(input, PlaybackPhase::Loading, false),
                TransportDecision::Notice(STILL_LOADING)
            );
        }
    }

    #[test]
    fn a_detached_track_has_no_playlist_to_navigate() {
        for input in [
            TransportInput::Previous,
            TransportInput::Next,
            TransportInput::Enter,
        ] {
            assert_eq!(
                decide_detached(input, PlaybackPhase::Playing, false),
                TransportDecision::Nothing
            );
        }
    }

    #[test]
    fn a_detached_live_stream_is_not_seeked() {
        assert_eq!(
            decide_detached(TransportInput::SeekBy(10), PlaybackPhase::Playing, true),
            TransportDecision::Notice(LIVE_NO_SEEK)
        );
        assert_eq!(
            decide_detached(TransportInput::Space, PlaybackPhase::Playing, true),
            TransportDecision::TogglePause
        );
    }
}
