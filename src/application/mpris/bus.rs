//! The `tenuto-mpris` thread: serves the MPRIS2 root and player interfaces
//! on the session bus from the latest published snapshot, and queues what
//! a desktop shell asks for. Handlers never wait on the runtime.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crossbeam_channel::Sender;
use mpris_server::zbus::{self, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface,
    Server, Time, TrackId,
};

use crate::lifecycle::panic::run_contained;

use super::cover::{clear_covers, write_cover};
use super::{Change, CoverUpdate, FailureLog, Published, Remote, Request, Status};

const NAME: &str = "tenuto";

pub(super) fn log(failure: FailureLog, what: &str, error: &dyn Display) {
    match failure {
        FailureLog::Warn => tracing::warn!(%error, "{what}"),
        FailureLog::Debug => tracing::debug!(%error, "{what}"),
    }
}

/// Runs `job` (the thread body) so a panic in it is one log line: under
/// `tui`'s hook an uncontained panic on any thread shuts the player down.
// ponytail: zbus's tokio integration runs some auth syscalls via
// spawn_blocking on blocking-pool threads, outside this guard; a panic there
// is still fatal under tui's hook. Low risk (trivial syscall wrappers);
// upgrade: contain at the hook level by thread name.
pub(super) fn contained(failure: FailureLog, job: impl FnOnce()) {
    if let Err(panic) = run_contained("mpris", job) {
        log(failure, "media controls stopped", &panic);
    }
}

/// Runs until the runtime drops its `MediaControls`, or the bus fails.
pub(super) fn serve(remote: Remote, failure: FailureLog, cover_dir: Option<PathBuf>) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(remote, failure, cover_dir)),
        Err(error) => log(failure, "media controls: cannot start a runtime", &error),
    }
}

async fn run(mut remote: Remote, failure: FailureLog, cover_dir: Option<PathBuf>) {
    if let Some(dir) = &cover_dir {
        clear_covers(dir);
    }
    let mut previous: Option<PathBuf> = None;
    let published = Arc::new(Mutex::new(Published::default()));
    let imp = || Imp {
        published: Arc::clone(&published),
        requests: remote.requests.clone(),
    };
    // A taken name is queued for, not refused, so ask the bus first.
    let primary = if name_taken().await {
        None
    } else {
        Server::new(NAME, imp()).await.ok()
    };
    let server = match primary {
        Some(server) => server,
        None => {
            let instance = format!("{NAME}.instance{}", std::process::id());
            match Server::new(&instance, imp()).await {
                Ok(server) => server,
                Err(error) => return log(failure, "media controls unavailable", &error),
            }
        }
    };
    while remote.snapshots.changed().await.is_ok() {
        let Some(snapshot) = remote.snapshots.borrow_and_update().clone() else {
            continue;
        };
        let changes = lock(&published).apply(snapshot);
        if let Some(dir) = &cover_dir {
            write_pending_cover(dir, &published, &mut previous);
        }
        let properties: Vec<Property> = {
            let published = lock(&published);
            changes
                .into_iter()
                .map(|c| property(&published, c))
                .collect()
        };
        if properties.is_empty() {
            continue;
        }
        if let Err(error) = server.properties_changed(properties).await {
            log(failure, "media controls: bus connection lost", &error);
            break;
        }
    }
    if let Some(path) = previous {
        let _ = std::fs::remove_file(path);
    }
}

/// Applies one pending cover update: a new cover is written (outside the
/// lock), the previous file deleted and `art_url` set to the new file's URL;
/// a cleared one deletes the file; a failed write leaves no `art_url`.
fn write_pending_cover(dir: &Path, published: &Mutex<Published>, previous: &mut Option<PathBuf>) {
    let update = lock(published).take_cover_update();
    let url = match update {
        CoverUpdate::Unchanged => return,
        CoverUpdate::Cleared => None,
        CoverUpdate::New { seq, image } => match write_cover(dir, seq, &image.0) {
            Ok(path) => {
                let url = url::Url::from_file_path(&path).ok().map(String::from);
                let stale = previous.replace(path);
                if let Some(stale) = stale {
                    let _ = std::fs::remove_file(stale);
                }
                url
            }
            Err(error) => {
                log(
                    FailureLog::Debug,
                    "media controls: cannot write the cover",
                    &error,
                );
                None
            }
        },
    };
    if url.is_none()
        && let Some(stale) = previous.take()
    {
        let _ = std::fs::remove_file(stale);
    }
    lock(published).set_art_url(url);
}

/// Whether another process owns `org.mpris.MediaPlayer2.tenuto`; false when
/// the bus can't say, so `Server::new` reports the real error.
async fn name_taken() -> bool {
    let Ok(connection) = zbus::Connection::session().await else {
        return false;
    };
    let Ok(proxy) = fdo::DBusProxy::new(&connection).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(format!("org.mpris.MediaPlayer2.{NAME}")) else {
        return false;
    };
    proxy.name_has_owner(name).await.unwrap_or(false)
}

fn lock(published: &Mutex<Published>) -> MutexGuard<'_, Published> {
    published.lock().unwrap_or_else(PoisonError::into_inner)
}

fn property(published: &Published, change: Change) -> Property {
    match change {
        Change::Metadata => Property::Metadata(metadata(published)),
        Change::PlaybackStatus => Property::PlaybackStatus(playback_status(published)),
        Change::Volume => Property::Volume(volume(published)),
    }
}

fn metadata(published: &Published) -> Metadata {
    let mut metadata = Metadata::new();
    let id = TrackId::try_from(published.track_id().as_str()).unwrap_or(TrackId::NO_TRACK);
    metadata.set_trackid(Some(id));
    if let Some(track) = published.snapshot().and_then(|s| s.track.as_ref()) {
        metadata.set_title(Some(track.title.clone()));
        metadata.set_artist(track.artist.clone().map(|a| [a]));
        metadata.set_length(track.length.map(|d| micros(d.as_micros())));
    }
    if let Some(url) = published.art_url() {
        metadata.set_art_url(Some(url));
    }
    metadata
}

fn micros(value: u128) -> Time {
    Time::from_micros(i64::try_from(value).unwrap_or(i64::MAX))
}

fn playback_status(published: &Published) -> PlaybackStatus {
    match published.snapshot().map(|s| s.status) {
        Some(Status::Playing) => PlaybackStatus::Playing,
        Some(Status::Paused) => PlaybackStatus::Paused,
        Some(Status::Stopped) | None => PlaybackStatus::Stopped,
    }
}

fn volume(published: &Published) -> f64 {
    published.snapshot().map_or(1.0, |s| s.volume)
}

struct Imp {
    published: Arc<Mutex<Published>>,
    requests: Sender<Request>,
}

impl Imp {
    fn published(&self) -> MutexGuard<'_, Published> {
        lock(&self.published)
    }

    /// Queues `request`; a full backlog (a click flood) drops it.
    fn send(&self, request: Request) -> fdo::Result<()> {
        let _dropped_when_full = self.requests.try_send(request);
        Ok(())
    }
}

impl RootInterface for Imp {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(NAME.to_owned())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Err(fdo::Error::NotSupported("no desktop entry".to_owned()))
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Imp {
    async fn next(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn previous(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Request::Pause)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Request::PlayPause)
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send(Request::Stop)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Request::Play)
    }

    async fn seek(&self, _offset: Time) -> fdo::Result<()> {
        Ok(())
    }

    async fn set_position(&self, _track_id: TrackId, _position: Time) -> fdo::Result<()> {
        Ok(())
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Ok(())
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status(&self.published()))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(LoopStatus::None)
    }

    async fn set_loop_status(&self, _loop_status: LoopStatus) -> zbus::Result<()> {
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_shuffle(&self, _shuffle: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(&self.published()))
    }

    async fn volume(&self) -> fdo::Result<f64> {
        Ok(volume(&self.published()))
    }

    async fn set_volume(&self, volume: f64) -> zbus::Result<()> {
        if volume.is_nan() {
            return Err(fdo::Error::InvalidArgs("volume is NaN".to_owned()).into());
        }
        self.published().mark_volume_written();
        self.send(Request::SetVolume(volume))?;
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        let position = self.published().snapshot().map(|s| s.position);
        Ok(position.map_or(Time::ZERO, |p| micros(p.as_micros())))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::mpris::{Change, MediaControls, Snapshot, Status, channel};
    use std::future::Future;
    use std::time::Duration;

    fn block<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn imp() -> (Imp, MediaControls) {
        let (controls, remote) = channel();
        let imp = Imp {
            published: Arc::new(Mutex::new(Published::default())),
            requests: remote.requests,
        };
        (imp, controls)
    }

    fn snap(position: u64) -> Snapshot {
        Snapshot {
            track: None,
            status: Status::Playing,
            volume: 1.0,
            position: Duration::from_secs(position),
            cover: None,
        }
    }

    #[test]
    fn a_panic_on_the_thread_is_contained() {
        contained(FailureLog::Debug, || panic!("bus thread panic"));
    }

    #[test]
    fn set_volume_rejects_nan() {
        let (imp, controls) = imp();
        let error = block(imp.set_volume(f64::NAN)).unwrap_err();
        assert!(
            matches!(&error, zbus::Error::FDO(e) if matches!(**e, fdo::Error::InvalidArgs(_))),
            "{error:?}"
        );
        assert!(controls.try_recv().is_none());
    }

    #[test]
    fn set_volume_queues_and_marks_written() {
        let (imp, controls) = imp();
        imp.published.lock().unwrap().apply(snap(0));
        block(imp.set_volume(0.5)).unwrap();
        assert_eq!(controls.try_recv(), Some(Request::SetVolume(0.5)));
        assert!(controls.try_recv().is_none());
        let changes = imp.published.lock().unwrap().apply(snap(0));
        assert_eq!(changes, vec![Change::Volume]);
    }

    #[test]
    fn pause_queues_pause_not_toggle() {
        let (imp, controls) = imp();
        block(imp.pause()).unwrap();
        assert_eq!(controls.try_recv(), Some(Request::Pause));
        assert!(controls.try_recv().is_none());
    }

    #[test]
    fn navigation_and_seek_are_ignored() {
        let (imp, controls) = imp();
        block(async {
            imp.next().await.unwrap();
            imp.previous().await.unwrap();
            imp.seek(Time::from_secs(5)).await.unwrap();
            imp.set_position(TrackId::NO_TRACK, Time::ZERO)
                .await
                .unwrap();
            assert!(!imp.can_go_next().await.unwrap());
            assert!(!imp.can_go_previous().await.unwrap());
            assert!(!imp.can_seek().await.unwrap());
        });
        assert!(controls.try_recv().is_none());
    }

    #[test]
    fn position_reads_the_latest_snapshot() {
        let (imp, _controls) = imp();
        imp.published.lock().unwrap().apply(snap(1));
        imp.published.lock().unwrap().apply(snap(7));
        assert_eq!(block(imp.position()).unwrap(), Time::from_secs(7));
    }

    #[test]
    fn metadata_has_no_track_without_a_track() {
        let (imp, _controls) = imp();
        imp.published.lock().unwrap().apply(snap(0));
        let metadata = block(imp.metadata()).unwrap();
        assert_eq!(metadata.trackid(), Some(TrackId::NO_TRACK));
    }

    fn with_cover(seq: u8) -> Snapshot {
        let image = image::DynamicImage::new_rgb8(4 + u32::from(seq), 4);
        Snapshot {
            cover: Some(crate::application::mpris::cover::CoverImage(Arc::new(
                image,
            ))),
            ..snap(0)
        }
    }

    /// Applies `snapshot` the way the serve loop does.
    fn step(
        dir: &std::path::Path,
        published: &Mutex<Published>,
        previous: &mut Option<std::path::PathBuf>,
        snapshot: Snapshot,
    ) {
        lock(published).apply(snapshot);
        write_pending_cover(dir, published, previous);
    }

    #[test]
    fn a_new_cover_is_written_and_published_as_a_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let (imp, _controls) = imp();
        let mut previous = None;
        step(dir.path(), &imp.published, &mut previous, with_cover(1));
        let path = previous.clone().unwrap();
        assert!(path.exists());
        let url = url::Url::from_file_path(&path).unwrap().to_string();
        assert_eq!(imp.published().art_url(), Some(url.as_str()));
        let metadata = block(imp.metadata()).unwrap();
        assert_eq!(metadata.art_url(), Some(url));
    }

    #[test]
    fn the_next_cover_deletes_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let (imp, _controls) = imp();
        let mut previous = None;
        step(dir.path(), &imp.published, &mut previous, with_cover(1));
        let first = previous.clone().unwrap();
        step(dir.path(), &imp.published, &mut previous, with_cover(2));
        let second = previous.clone().unwrap();
        assert_ne!(first, second);
        let url = url::Url::from_file_path(&second).unwrap().to_string();
        assert_eq!(imp.published().art_url(), Some(url.as_str()));
        assert!(!first.exists());
        assert!(second.exists());
    }

    #[test]
    fn a_cleared_cover_deletes_the_file_and_drops_art_url() {
        let dir = tempfile::tempdir().unwrap();
        let (imp, _controls) = imp();
        let mut previous = None;
        step(dir.path(), &imp.published, &mut previous, with_cover(1));
        let path = previous.clone().unwrap();
        step(dir.path(), &imp.published, &mut previous, snap(0));
        assert!(!path.exists());
        assert!(previous.is_none());
        assert_eq!(imp.published().art_url(), None);
        assert_eq!(block(imp.metadata()).unwrap().art_url(), None::<String>);
    }

    #[test]
    fn an_unwritable_cover_dir_leaves_no_art_url() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let (imp, _controls) = imp();
        let mut previous = None;
        step(
            &file.join("sub"),
            &imp.published,
            &mut previous,
            with_cover(1),
        );
        assert!(previous.is_none());
        assert_eq!(imp.published().art_url(), None);
    }

    #[test]
    fn a_failed_write_after_a_cover_deletes_it_and_drops_art_url() {
        let dir = tempfile::tempdir().unwrap();
        let (imp, _controls) = imp();
        let mut previous = None;
        step(dir.path(), &imp.published, &mut previous, with_cover(1));
        let first = previous.clone().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        step(
            &blocker.join("sub"),
            &imp.published,
            &mut previous,
            with_cover(2),
        );
        assert!(!first.exists());
        assert!(previous.is_none());
        assert_eq!(imp.published().art_url(), None);
    }
}
