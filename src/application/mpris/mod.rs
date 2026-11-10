//! The platform-independent half of the MPRIS media controls: what a
//! desktop shell is told (`Snapshot`), what it may ask (`Request`), which
//! properties changed between two snapshots, and the channel pair between
//! the runtime and the D-Bus thread. No D-Bus code lives here.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use self::cover::CoverImage;

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::watch;

use crate::application::runtime::AppCommand;
use crate::application::transport::PlaybackPhase;
use crate::application::view::{PlayerView, filled};
use crate::media::id::MediaId;
use crate::playlist::RepeatMode;

pub mod cover;

#[cfg(target_os = "linux")]
mod bus;

const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub media: MediaId,
    pub title: String,
    pub artist: Option<String>,
    pub length: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub track: Option<Track>,
    pub status: Status,
    pub volume: f64,
    pub position: Duration,
    pub cover: Option<CoverImage>,
    /// The playing playlist is shuffled.
    pub shuffle: bool,
    /// Winamp's repeat toggle (M13 §1.1), published as MPRIS `LoopStatus`:
    /// off → `None`, one → `Track`, all → `Playlist`.
    pub repeat: RepeatMode,
    /// The pass this snapshot came from executed a seek — a desktop
    /// request or the player's own keys, both through the one
    /// `transport` door — so its jump is real, reported even when the
    /// pass followed a front-end stall wider than the seek window.
    /// Provenance the bus thread cannot recover on its own, like
    /// `Published`'s `volume_written`; the runtime sets it,
    /// [`snapshot`] never does.
    pub sought: bool,
}

fn clean(text: &str) -> String {
    text.replace('\0', "")
}

/// What the desktop should see for `view`. `media` is the identity of the
/// playing entry and `podcast` the feed name, both resolved by the caller.
pub fn snapshot(view: &PlayerView, media: Option<&MediaId>, podcast: Option<&str>) -> Snapshot {
    let track = match (&view.now_playing, media) {
        (Some(now), Some(media)) => Some(Track {
            media: media.clone(),
            title: clean(&now.title),
            artist: filled(podcast.or(now.artist.as_deref())).map(clean),
            length: if view.live {
                None
            } else {
                now.duration.map(|d| d.value)
            },
        }),
        _ => None,
    };
    let status = match view.phase {
        PlaybackPhase::Playing | PlaybackPhase::Reconnecting | PlaybackPhase::Loading => {
            Status::Playing
        }
        PlaybackPhase::Paused => Status::Paused,
        _ => Status::Stopped,
    };
    Snapshot {
        track,
        status,
        volume: f64::from(view.volume.as_gain()),
        position: view
            .now_playing
            .as_ref()
            .map_or(Duration::ZERO, |n| n.position),
        cover: None,
        shuffle: view.tabs.iter().any(|t| t.playing && t.shuffled),
        repeat: view.repeat,
        sought: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// MPRIS `Seek`: a relative offset in microseconds.
    Seek(i64),
    SetPosition(Duration),
    SetShuffle(bool),
    SetLoopStatus(RepeatMode),
    SetVolume(f64),
}

pub fn command(request: Request, view: &PlayerView) -> Option<AppCommand> {
    match request {
        Request::PlayPause => Some(AppCommand::PlayPause),
        Request::Play => Some(AppCommand::Play),
        Request::Pause => Some(AppCommand::Pause),
        Request::Stop => Some(AppCommand::Stop),
        Request::Next => Some(AppCommand::Next),
        Request::Previous => Some(AppCommand::Previous),
        Request::Seek(micros) => {
            let secs = (micros as f64 / 1e6).round() as i64;
            (secs != 0).then_some(AppCommand::SeekBy(secs))
        }
        Request::SetPosition(position) => Some(AppCommand::SeekTo(position)),
        Request::SetShuffle(on) => view
            .tabs
            .iter()
            .find(|t| t.playing && t.shuffled != on)
            .map(|t| AppCommand::ToggleShuffle(t.id)),
        Request::SetLoopStatus(mode) => {
            (view.repeat != mode).then_some(AppCommand::SetRepeat(mode))
        }
        Request::SetVolume(x) if x.is_nan() => None,
        Request::SetVolume(x) => {
            let delta = x.clamp(0.0, 1.0) - f64::from(view.volume.as_gain());
            (delta != 0.0).then_some(AppCommand::AdjustVolume(delta as f32))
        }
    }
}

/// How far the position may stray from where playback alone would have
/// taken it before clients are told it was seeked.
const SEEK_TOLERANCE: Duration = Duration::from_secs(1);

/// The widest gap between two snapshots that playback is still extrapolated
/// over when looking for a seek — a few front-end pumps at any honest
/// cadence. Every seek the runtime executes marks the snapshot that carries
/// it (`Snapshot::sought`), from the desktop's widget and the player's own
/// keys alike, so the extrapolation is only a backstop for jumps nothing
/// commanded: a resume seek landing inside an already-loaded track. At pump
/// cadence such a landing shows up within a pump or two. A wider gap means
/// the front end stalled between pumps, and playback drifts from wall time
/// for honest reasons: a rebuffer, an underrun, the track ending. The fresh
/// position is the truth there; extrapolating across the stall reports a
/// phantom seek.
///
/// Must stay well under `SEEK_TOLERANCE`: inside the window, drift can
/// stray from the wall clock by at most the window itself, so only a jump
/// far larger than any drift trips the check; past it, no extrapolation
/// happens at all. `the_seek_window_never_widens_past_the_tolerance` pins
/// the bound.
const SEEK_WINDOW: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Metadata,
    PlaybackStatus,
    Volume,
    Shuffle,
    LoopStatus,
    /// The position jumped: a seek, from MPRIS or the player itself.
    Seeked,
}

/// The last snapshot handed to the desktop, and the track-id sequence.
#[derive(Default)]
pub struct Published {
    last: Option<Snapshot>,
    last_at: Option<Instant>,
    seq: u64,
    volume_written: bool,
    cover_seq: u64,
    cover_taken: Option<CoverImage>,
    art_url: Option<String>,
}

/// What `Published::take_cover_update` found since the last call.
pub enum CoverUpdate {
    Unchanged,
    Cleared,
    New { seq: u64, image: CoverImage },
}

impl Published {
    pub fn apply(&mut self, next: Snapshot) -> Vec<Change> {
        self.apply_at(next, Instant::now())
    }

    /// `apply`, with `now` the time `next` was taken.
    pub fn apply_at(&mut self, next: Snapshot, now: Instant) -> Vec<Change> {
        let mut changes = Vec::new();
        let last = self.last.as_ref();
        let media = |s: &Snapshot| s.track.as_ref().map(|t| t.media.clone());
        if last.map(media) != Some(media(&next)) && !(last.is_none() && next.track.is_none()) {
            self.seq += 1;
        }
        if last.is_none_or(|l| l.track != next.track || l.cover != next.cover) {
            changes.push(Change::Metadata);
        }
        if last.is_none_or(|l| l.status != next.status) {
            changes.push(Change::PlaybackStatus);
        }
        if last.is_none_or(|l| l.volume != next.volume) || self.volume_written {
            changes.push(Change::Volume);
        }
        if last.is_none_or(|l| l.shuffle != next.shuffle) {
            changes.push(Change::Shuffle);
        }
        if last.is_none_or(|l| l.repeat != next.repeat) {
            changes.push(Change::LoopStatus);
        }
        // A seek the runtime drained is reported whatever the gap since
        // the last snapshot: a widget seek queued while the front end was
        // stalled lands in the first post-stall pass, outside the window,
        // and the window would otherwise hide a real seek in exactly the
        // regime it cannot judge.
        let mut seeked = next.sought;
        if !seeked
            && let (Some(l), Some(at)) = (last, self.last_at)
            && media(l) == media(&next)
        {
            let elapsed = now.saturating_duration_since(at);
            let expected = match l.status {
                Status::Playing if elapsed <= SEEK_WINDOW => Some(l.position + elapsed),
                // Beyond the window the front end stalled; the fresh
                // position is the truth, whatever the wall clock says.
                Status::Playing => None,
                _ => Some(l.position),
            };
            seeked =
                expected.is_some_and(|expected| expected.abs_diff(next.position) > SEEK_TOLERANCE);
        }
        if seeked {
            changes.push(Change::Seeked);
        }
        self.volume_written = false;
        self.last = Some(next);
        self.last_at = Some(now);
        changes
    }

    /// Reports each change of the snapshot's cover once; `seq` counts covers.
    /// Call it after every `apply`, or a cover change is merged into the next.
    pub fn take_cover_update(&mut self) -> CoverUpdate {
        let now = self.last.as_ref().and_then(|s| s.cover.clone());
        if now == self.cover_taken {
            return CoverUpdate::Unchanged;
        }
        self.cover_taken = now.clone();
        match now {
            Some(image) => {
                self.cover_seq += 1;
                CoverUpdate::New {
                    seq: self.cover_seq,
                    image,
                }
            }
            None => CoverUpdate::Cleared,
        }
    }

    pub fn set_art_url(&mut self, url: Option<String>) {
        self.art_url = url;
    }

    pub fn art_url(&self) -> Option<&str> {
        self.art_url.as_deref()
    }

    pub fn track_id(&self) -> String {
        match self.last.as_ref().and_then(|s| s.track.as_ref()) {
            Some(_) => format!("/org/tenuto/track/{}", self.seq),
            None => NO_TRACK.to_owned(),
        }
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.last.as_ref()
    }

    /// A client wrote `Volume`: report it even if the value ended unchanged.
    pub fn mark_volume_written(&mut self) {
        self.volume_written = true;
    }
}

/// Where the D-Bus thread logs its one failure line: `tui` sends `warn` to
/// its log file, `play` keeps stderr quiet with `debug`.
#[derive(Clone, Copy, Debug)]
pub enum FailureLog {
    Warn,
    Debug,
}

/// Starts the `tenuto-mpris` thread and returns at once; connecting happens
/// on the thread. `None` when `TENUTO_MPRIS=off`, off Linux, or the thread
/// could not be spawned. `cover_dir` is where cover PNGs are written; `None`
/// publishes no covers.
pub fn spawn(failure: FailureLog, cover_dir: Option<PathBuf>) -> Option<MediaControls> {
    if std::env::var_os("TENUTO_MPRIS").is_some_and(|v| v == "off") {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let (controls, remote) = channel();
        let spawned = std::thread::Builder::new()
            .name("tenuto-mpris".into())
            .spawn(move || bus::contained(failure, || bus::serve(remote, failure, cover_dir)));
        match spawned {
            Ok(_detached) => Some(controls),
            Err(error) => {
                bus::log(failure, "media controls: cannot start a thread", &error);
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (failure, cover_dir);
        None
    }
}

pub const REQUEST_BACKLOG: usize = 16;

/// The runtime's end: drains requests, publishes snapshots.
pub struct MediaControls {
    requests: Receiver<Request>,
    snapshots: watch::Sender<Option<Snapshot>>,
}

impl MediaControls {
    pub fn try_recv(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// The D-Bus thread has gone: nobody reads snapshots any more.
    pub fn is_closed(&self) -> bool {
        self.snapshots.is_closed()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        self.snapshots.send_replace(Some(snapshot));
    }
}

/// The D-Bus thread's end.
pub struct Remote {
    pub requests: Sender<Request>,
    pub snapshots: watch::Receiver<Option<Snapshot>>,
}

pub fn channel() -> (MediaControls, Remote) {
    let (request_tx, request_rx) = crossbeam_channel::bounded(REQUEST_BACKLOG);
    let (snapshot_tx, snapshot_rx) = watch::channel(None);
    (
        MediaControls {
            requests: request_rx,
            snapshots: snapshot_tx,
        },
        Remote {
            requests: request_tx,
            snapshots: snapshot_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::view::PlaylistTab;
    use crate::application::view::{NowPlaying, PersistenceStatus};
    use crate::media::id::AbsolutePath;
    use crate::persistence::model::PersistedState;
    use crate::playlist::queue::{DisplayDuration, DurationSource};
    use crate::volume::Volume;
    use std::sync::Arc;

    fn view(phase: PlaybackPhase, now: Option<NowPlaying>) -> PlayerView {
        let viewed = PersistedState::default().playlists().playing();
        PlayerView {
            rows: Arc::new(Vec::new()),
            tabs: Vec::new(),
            viewed,
            active: None,
            now_playing: now,
            phase,
            volume: Volume::new(0.75),
            repeat: RepeatMode::Off,
            status: None,
            persistence: PersistenceStatus::Saving,
            last_requested: None,
            live: false,
            reconnecting: false,
        }
    }

    fn media(name: &str) -> MediaId {
        MediaId::LocalFile(AbsolutePath::new(format!("/{name}").into()).unwrap())
    }

    fn now(title: &str) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            loaded: true,
            ..NowPlaying::unloaded()
        }
    }

    fn snap(track: Option<Track>, status: Status, volume: f64) -> Snapshot {
        Snapshot {
            track,
            status,
            volume,
            position: Duration::ZERO,
            cover: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            sought: false,
        }
    }

    fn track(name: &str) -> Track {
        Track {
            media: media(name),
            title: name.into(),
            artist: None,
            length: None,
        }
    }

    #[test]
    fn snapshot_maps_each_phase() {
        use PlaybackPhase::*;
        let cases = [
            (Unloaded, Status::Stopped),
            (Loading, Status::Playing),
            (LoadFailed, Status::Stopped),
            (Playing, Status::Playing),
            (Reconnecting, Status::Playing),
            (Paused, Status::Paused),
            (Stopped, Status::Stopped),
            (Ended, Status::Stopped),
        ];
        for (phase, status) in cases {
            let s = snapshot(&view(phase, Some(now("t"))), Some(&media("a")), None);
            assert_eq!(s.status, status, "{phase:?}");
        }
        // An unloaded active entry is stopped but keeps its track.
        let unloaded = NowPlaying {
            title: "t".into(),
            ..NowPlaying::unloaded()
        };
        let s = snapshot(&view(Unloaded, Some(unloaded)), Some(&media("a")), None);
        assert_eq!(s.status, Status::Stopped);
        assert!(s.track.is_some());
        assert_eq!(s.volume, 0.75);
    }

    #[test]
    fn snapshot_prefers_podcast_name_over_tag_artist() {
        let mut n = now("t");
        n.artist = Some("tag".into());
        let v = view(PlaybackPhase::Playing, Some(n));
        let a = |p| snapshot(&v, Some(&media("a")), p).track.unwrap().artist;
        assert_eq!(a(Some("Feed")), Some("Feed".into()));
        assert_eq!(a(None), Some("tag".into()));
    }

    #[test]
    fn snapshot_omits_length_for_live_and_unknown_durations() {
        let mut n = now("t");
        n.duration = Some(DisplayDuration {
            value: Duration::from_secs(90),
            source: DurationSource::Declared,
        });
        let mut v = view(PlaybackPhase::Playing, Some(n.clone()));
        let len = |v: &PlayerView| snapshot(v, Some(&media("a")), None).track.unwrap().length;
        assert_eq!(len(&v), Some(Duration::from_secs(90)));
        v.live = true;
        assert_eq!(len(&v), None);
        n.duration = None;
        assert_eq!(len(&view(PlaybackPhase::Playing, Some(n))), None);
    }

    #[test]
    fn snapshot_strips_nul_from_title_and_artist() {
        let mut n = now("a\0b");
        n.artist = Some("c\0d".into());
        let v = view(PlaybackPhase::Playing, Some(n));
        let t = snapshot(&v, Some(&media("a")), None).track.unwrap();
        assert_eq!(t.title, "ab");
        assert_eq!(t.artist.as_deref(), Some("cd"));
    }

    #[test]
    fn snapshot_omits_an_empty_or_blank_artist() {
        for blank in ["", "  "] {
            let mut n = now("t");
            n.artist = Some(blank.into());
            let v = view(PlaybackPhase::Playing, Some(n));
            let t = snapshot(&v, Some(&media("a")), None).track.unwrap();
            assert_eq!(t.artist, None, "{blank:?}");
        }
    }

    #[test]
    fn snapshot_without_now_playing_has_no_track() {
        let s = snapshot(&view(PlaybackPhase::Stopped, None), Some(&media("a")), None);
        assert_eq!(s.track, None);
        assert_eq!(s.position, Duration::ZERO);
        let v = view(PlaybackPhase::Playing, Some(now("t")));
        assert_eq!(snapshot(&v, None, None).track, None);
    }

    fn at_volume(volume: f32) -> PlayerView {
        PlayerView {
            volume: Volume::new(volume),
            ..view(PlaybackPhase::Playing, None)
        }
    }

    #[test]
    fn command_maps_transport_requests() {
        let v = &at_volume(1.0);
        assert!(matches!(
            command(Request::PlayPause, v),
            Some(AppCommand::PlayPause)
        ));
        assert!(matches!(command(Request::Play, v), Some(AppCommand::Play)));
        assert!(matches!(
            command(Request::Pause, v),
            Some(AppCommand::Pause)
        ));
        assert!(matches!(command(Request::Stop, v), Some(AppCommand::Stop)));
        assert!(matches!(command(Request::Next, v), Some(AppCommand::Next)));
        assert!(matches!(
            command(Request::Previous, v),
            Some(AppCommand::Previous)
        ));
    }

    #[test]
    fn command_seek_rounds_to_whole_seconds() {
        let v = &at_volume(1.0);
        assert!(matches!(
            command(Request::Seek(-10_000_000), v),
            Some(AppCommand::SeekBy(-10))
        ));
        assert!(matches!(
            command(Request::Seek(1_600_000), v),
            Some(AppCommand::SeekBy(2))
        ));
        assert!(command(Request::Seek(400_000), v).is_none());
        let to = Duration::from_secs(42);
        assert!(matches!(
            command(Request::SetPosition(to), v),
            Some(AppCommand::SeekTo(d)) if d == to
        ));
    }

    #[test]
    fn command_set_shuffle_toggles_the_playing_playlist_only_on_change() {
        let mut v = at_volume(1.0);
        let id = v.viewed;
        v.tabs = vec![PlaylistTab {
            id,
            name: "p".into(),
            playing: true,
            shuffled: false,
        }];
        assert!(command(Request::SetShuffle(false), &v).is_none());
        assert!(matches!(
            command(Request::SetShuffle(true), &v),
            Some(AppCommand::ToggleShuffle(t)) if t == id
        ));
        assert!(!snapshot(&v, None, None).shuffle);
        v.tabs[0].shuffled = true;
        assert!(snapshot(&v, None, None).shuffle);
        assert!(command(Request::SetShuffle(true), &v).is_none());
    }

    #[test]
    fn command_volume_rejects_nan_and_clamps() {
        let full = &at_volume(1.0);
        assert!(command(Request::SetVolume(f64::NAN), full).is_none());
        assert!(command(Request::SetVolume(1.5), full).is_none());
        match command(Request::SetVolume(-1.0), &at_volume(0.5)) {
            Some(AppCommand::AdjustVolume(d)) => assert_eq!(d, -0.5),
            other => panic!("{other:?}"),
        }
        match command(Request::SetVolume(0.25), &at_volume(0.75)) {
            Some(AppCommand::AdjustVolume(d)) => assert_eq!(d, -0.5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_position_jump_reports_seeked_and_playback_does_not() {
        let mut p = Published::default();
        let t0 = Instant::now();
        let at = |secs: u64, status: Status| {
            let mut s = snap(Some(track("a")), status, 1.0);
            s.position = Duration::from_secs(secs);
            s
        };
        p.apply_at(at(10, Status::Playing), t0);
        let later = |secs| t0 + Duration::from_secs(secs);
        assert!(p.apply_at(at(13, Status::Playing), later(3)).is_empty());
        // At pump cadence a jump is a seek and is reported; a jump after a
        // stall-wide gap is not: the fresh position is the truth there, and
        // any seek the runtime executed marks its own snapshot instead.
        assert_eq!(
            p.apply_at(
                at(60, Status::Playing),
                later(3) + Duration::from_millis(250)
            ),
            vec![Change::Seeked]
        );
        p.apply_at(at(60, Status::Paused), later(4));
        assert!(p.apply_at(at(60, Status::Paused), later(30)).is_empty());
        assert_eq!(
            p.apply_at(at(5, Status::Paused), later(31)),
            vec![Change::Seeked]
        );
        // A new track starting elsewhere is metadata, not a seek.
        let mut other = snap(Some(track("b")), Status::Paused, 1.0);
        other.position = Duration::from_secs(90);
        assert_eq!(p.apply_at(other, later(32)), vec![Change::Metadata]);
    }

    #[test]
    fn a_stalled_front_end_does_not_report_a_phantom_seeked() {
        let mut p = Published::default();
        let t0 = Instant::now();
        let playing = |secs: u64| {
            let mut s = snap(Some(track("a")), Status::Playing, 1.0);
            s.position = Duration::from_secs(secs);
            s
        };
        p.apply_at(playing(10), t0);
        // At pump cadence, playback tracking the wall clock is no seek.
        assert!(
            p.apply_at(playing(10), t0 + Duration::from_millis(100))
                .is_empty()
        );
        // The front end stalls 30 s between pumps. Playback carried on, but
        // the stream rebuffered inside the stall, so the position is behind
        // the wall clock; that divergence is not a seek.
        let resumed = t0 + Duration::from_secs(30);
        let changes = p.apply_at(playing(12), resumed);
        assert!(!changes.contains(&Change::Seeked), "{changes:?}");
        // Back at pump cadence, a real jump still reports one.
        assert_eq!(
            p.apply_at(playing(60), resumed + Duration::from_millis(100)),
            vec![Change::Seeked]
        );
    }

    #[test]
    fn a_seek_drained_through_a_stall_still_reports_seeked() {
        let mut p = Published::default();
        let t0 = Instant::now();
        let playing = |secs: u64, sought: bool| {
            let mut s = snap(Some(track("a")), Status::Playing, 1.0);
            s.position = Duration::from_secs(secs);
            s.sought = sought;
            s
        };
        p.apply_at(playing(10, false), t0);
        // The widget seeks while the front end is stalled. The runtime
        // drains the request on the first pump after and marks that
        // snapshot, so the jump is reported despite the stall's width.
        let resumed = t0 + Duration::from_secs(30);
        assert_eq!(p.apply_at(playing(42, true), resumed), vec![Change::Seeked]);
        // The mark is spent: a later stall with no seek reports nothing.
        assert!(
            p.apply_at(playing(43, false), resumed + Duration::from_secs(30))
                .is_empty()
        );
    }

    #[test]
    fn a_stall_the_old_window_covered_does_not_report_a_phantom_seeked() {
        let mut p = Published::default();
        let t0 = Instant::now();
        let playing = |secs: u64| {
            let mut s = snap(Some(track("a")), Status::Playing, 1.0);
            s.position = Duration::from_secs(secs);
            s
        };
        p.apply_at(playing(10), t0);
        // A one-second stall — a width the window once extrapolated over —
        // with a rebuffer that left the position half a second behind:
        // divergence between wall clock and position, not a seek.
        let changes = p.apply_at(playing(9), t0 + Duration::from_secs(1));
        assert!(!changes.contains(&Change::Seeked), "{changes:?}");
    }

    #[test]
    fn an_in_window_jump_far_beyond_drift_still_reports_seeked() {
        let mut p = Published::default();
        let t0 = Instant::now();
        let playing = |secs: u64| {
            let mut s = snap(Some(track("a")), Status::Playing, 1.0);
            s.position = Duration::from_secs(secs);
            s
        };
        p.apply_at(playing(10), t0);
        // Within the window no drift can stray a second from the wall
        // clock, so a two-second backward jump in one pump's width is a
        // real seek and is reported.
        assert_eq!(
            p.apply_at(playing(8), t0 + Duration::from_millis(250)),
            vec![Change::Seeked]
        );
    }

    #[test]
    fn the_seek_window_never_widens_past_the_tolerance() {
        assert!(SEEK_WINDOW <= SEEK_TOLERANCE);
    }

    #[test]
    fn shuffle_change_reports_shuffle() {
        let mut p = Published::default();
        p.apply(snap(None, Status::Playing, 1.0));
        let mut next = snap(None, Status::Playing, 1.0);
        next.shuffle = true;
        assert_eq!(p.apply(next), vec![Change::Shuffle]);
    }

    #[test]
    fn first_apply_reports_everything() {
        let mut p = Published::default();
        let all = vec![
            Change::Metadata,
            Change::PlaybackStatus,
            Change::Volume,
            Change::Shuffle,
            Change::LoopStatus,
        ];
        assert_eq!(p.apply(snap(None, Status::Stopped, 1.0)), all);
    }

    #[test]
    fn position_only_change_reports_nothing_but_updates_position() {
        let mut p = Published::default();
        let t0 = Instant::now();
        p.apply_at(snap(Some(track("a")), Status::Playing, 1.0), t0);
        let mut next = snap(Some(track("a")), Status::Playing, 1.0);
        next.position = Duration::from_secs(5);
        assert!(p.apply_at(next, t0 + Duration::from_secs(5)).is_empty());
        assert_eq!(p.snapshot().unwrap().position, Duration::from_secs(5));
    }

    #[test]
    fn new_media_bumps_track_id() {
        let mut p = Published::default();
        p.apply(snap(Some(track("a")), Status::Playing, 1.0));
        let first = p.track_id();
        p.apply(snap(Some(track("b")), Status::Playing, 1.0));
        assert_ne!(p.track_id(), first);
        assert!(p.track_id().starts_with("/org/tenuto/track/"));
    }

    #[test]
    fn same_media_new_length_keeps_track_id_and_reports_metadata() {
        let mut p = Published::default();
        p.apply(snap(Some(track("a")), Status::Playing, 1.0));
        let id = p.track_id();
        let mut longer = track("a");
        longer.length = Some(Duration::from_secs(9));
        assert_eq!(
            p.apply(snap(Some(longer), Status::Playing, 1.0)),
            vec![Change::Metadata]
        );
        assert_eq!(p.track_id(), id);
    }

    #[test]
    fn volume_write_reports_volume_even_when_unchanged() {
        let mut p = Published::default();
        p.apply(snap(None, Status::Stopped, 1.0));
        p.mark_volume_written();
        assert_eq!(
            p.apply(snap(None, Status::Stopped, 1.0)),
            vec![Change::Volume]
        );
        assert!(p.apply(snap(None, Status::Stopped, 1.0)).is_empty());
    }

    #[test]
    fn status_only_change_reports_playback_status() {
        let mut p = Published::default();
        p.apply(snap(None, Status::Playing, 1.0));
        assert_eq!(
            p.apply(snap(None, Status::Paused, 1.0)),
            vec![Change::PlaybackStatus]
        );
    }

    #[test]
    fn volume_only_change_reports_volume() {
        let mut p = Published::default();
        p.apply(snap(None, Status::Playing, 1.0));
        assert_eq!(
            p.apply(snap(None, Status::Playing, 0.5)),
            vec![Change::Volume]
        );
    }

    #[test]
    fn track_id_is_no_track_without_a_track() {
        let mut p = Published::default();
        assert_eq!(p.track_id(), NO_TRACK);
        p.apply(snap(Some(track("a")), Status::Playing, 1.0));
        p.apply(snap(None, Status::Stopped, 1.0));
        assert_eq!(p.track_id(), NO_TRACK);
    }

    #[test]
    fn same_media_new_cover_reports_metadata_and_keeps_track_id() {
        let mut p = Published::default();
        p.apply(snap(Some(track("a")), Status::Playing, 1.0));
        let id = p.track_id();
        let mut next = snap(Some(track("a")), Status::Playing, 1.0);
        next.cover = Some(cover());
        assert_eq!(p.apply(next), vec![Change::Metadata]);
        assert_eq!(p.track_id(), id);
    }

    #[test]
    fn take_cover_update_reports_each_change_once() {
        let mut p = Published::default();
        let with = |c: Option<CoverImage>| {
            let mut s = snap(Some(track("a")), Status::Playing, 1.0);
            s.cover = c;
            s
        };
        let first = cover();
        p.apply(with(Some(first.clone())));
        assert!(matches!(
            p.take_cover_update(),
            CoverUpdate::New { seq: 1, .. }
        ));
        assert!(matches!(p.take_cover_update(), CoverUpdate::Unchanged));
        p.apply(with(Some(cover())));
        assert!(matches!(
            p.take_cover_update(),
            CoverUpdate::New { seq: 2, .. }
        ));
        p.apply(with(None));
        assert!(matches!(p.take_cover_update(), CoverUpdate::Cleared));
        assert!(matches!(p.take_cover_update(), CoverUpdate::Unchanged));
    }

    fn cover() -> CoverImage {
        CoverImage(Arc::new(image::DynamicImage::new_rgb8(1, 1)))
    }

    #[test]
    fn click_flood_drops_beyond_backlog() {
        let (controls, remote) = channel();
        let sent = (0..20).filter(|_| remote.requests.try_send(Request::PlayPause).is_ok());
        assert_eq!(sent.count(), REQUEST_BACKLOG);
        let mut got = 0;
        while controls.try_recv().is_some() {
            got += 1;
        }
        assert_eq!(got, 16);
        assert!(controls.try_recv().is_none());
    }
}
