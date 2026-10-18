//! The platform-independent half of the MPRIS media controls: what a
//! desktop shell is told (`Snapshot`), what it may ask (`Request`), which
//! properties changed between two snapshots, and the channel pair between
//! the runtime and the D-Bus thread. No D-Bus code lives here.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::watch;

use crate::application::runtime::AppCommand;
use crate::application::transport::PlaybackPhase;
use crate::application::view::PlayerView;
use crate::media::id::MediaId;
use crate::volume::Volume;

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
            artist: podcast.or(now.artist.as_deref()).map(clean),
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
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    PlayPause,
    Play,
    Pause,
    Stop,
    SetVolume(f64),
}

pub fn command(request: Request, volume: Volume) -> Option<AppCommand> {
    match request {
        Request::PlayPause => Some(AppCommand::PlayPause),
        Request::Play => Some(AppCommand::Play),
        Request::Pause => Some(AppCommand::Pause),
        Request::Stop => Some(AppCommand::Stop),
        Request::SetVolume(x) if x.is_nan() => None,
        Request::SetVolume(x) => {
            let delta = x.clamp(0.0, 1.0) - f64::from(volume.as_gain());
            (delta != 0.0).then_some(AppCommand::AdjustVolume(delta as f32))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Metadata,
    PlaybackStatus,
    Volume,
}

/// The last snapshot handed to the desktop, and the track-id sequence.
#[derive(Default)]
pub struct Published {
    last: Option<Snapshot>,
    seq: u64,
    volume_written: bool,
}

impl Published {
    pub fn apply(&mut self, next: Snapshot) -> Vec<Change> {
        let mut changes = Vec::new();
        let last = self.last.as_ref();
        let media = |s: &Snapshot| s.track.as_ref().map(|t| t.media.clone());
        if last.map(media) != Some(media(&next)) && !(last.is_none() && next.track.is_none()) {
            self.seq += 1;
        }
        if last.is_none_or(|l| l.track != next.track) {
            changes.push(Change::Metadata);
        }
        if last.is_none_or(|l| l.status != next.status) {
            changes.push(Change::PlaybackStatus);
        }
        if last.is_none_or(|l| l.volume != next.volume) || self.volume_written {
            changes.push(Change::Volume);
        }
        self.volume_written = false;
        self.last = Some(next);
        changes
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
/// could not be spawned.
pub fn spawn(failure: FailureLog) -> Option<MediaControls> {
    if std::env::var_os("TENUTO_MPRIS").is_some_and(|v| v == "off") {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let (controls, remote) = channel();
        let spawned = std::thread::Builder::new()
            .name("tenuto-mpris".into())
            .spawn(move || bus::serve(remote, failure));
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
        let _ = failure;
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
    use crate::application::view::{NowPlaying, PersistenceStatus};
    use crate::media::id::AbsolutePath;
    use crate::persistence::model::PersistedState;
    use crate::playlist::queue::{DisplayDuration, DurationSource};
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
    fn snapshot_without_now_playing_has_no_track() {
        let s = snapshot(&view(PlaybackPhase::Stopped, None), Some(&media("a")), None);
        assert_eq!(s.track, None);
        assert_eq!(s.position, Duration::ZERO);
        let v = view(PlaybackPhase::Playing, Some(now("t")));
        assert_eq!(snapshot(&v, None, None).track, None);
    }

    #[test]
    fn command_maps_transport_requests() {
        let v = Volume::FULL;
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
    }

    #[test]
    fn command_volume_rejects_nan_and_clamps() {
        assert!(command(Request::SetVolume(f64::NAN), Volume::FULL).is_none());
        assert!(command(Request::SetVolume(1.5), Volume::FULL).is_none());
        match command(Request::SetVolume(-1.0), Volume::new(0.5)) {
            Some(AppCommand::AdjustVolume(d)) => assert_eq!(d, -0.5),
            other => panic!("{other:?}"),
        }
        match command(Request::SetVolume(0.25), Volume::new(0.75)) {
            Some(AppCommand::AdjustVolume(d)) => assert_eq!(d, -0.5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn first_apply_reports_everything() {
        let mut p = Published::default();
        let all = vec![Change::Metadata, Change::PlaybackStatus, Change::Volume];
        assert_eq!(p.apply(snap(None, Status::Stopped, 1.0)), all);
    }

    #[test]
    fn position_only_change_reports_nothing_but_updates_position() {
        let mut p = Published::default();
        p.apply(snap(Some(track("a")), Status::Playing, 1.0));
        let mut next = snap(Some(track("a")), Status::Playing, 1.0);
        next.position = Duration::from_secs(5);
        assert!(p.apply(next).is_empty());
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
