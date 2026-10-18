//! Cover art for `mpris:artUrl`: a cheaply comparable image handle and the
//! PNG file a desktop shell reads it from.

use std::fmt;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{DynamicImage, ImageFormat};

use crate::artwork::worker::{ArtworkWorker, CoverLoader, CoverSource};
use crate::media::id::MediaId;

pub const COVER_EDGE: u32 = 512;

/// Equal only to its own clones, so a `Snapshot` stays `PartialEq`.
#[derive(Clone)]
pub struct CoverImage(pub Arc<DynamicImage>);

impl PartialEq for CoverImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for CoverImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CoverImage({}x{})", self.0.width(), self.0.height())
    }
}

/// Fetches the playing media's cover on the artwork worker.
// ponytail: in `tui`, a remote cover is fetched again here beside
// `tui::Artwork`. If that matters, share the fetch by having the TUI hand its
// decoded image to the runtime.
pub struct CoverFetcher {
    worker: ArtworkWorker,
    requested: Option<(MediaId, CoverSource)>,
    current: Option<(MediaId, CoverImage)>,
}

impl CoverFetcher {
    pub fn new(loader: CoverLoader) -> Self {
        Self {
            worker: ArtworkWorker::spawn(loader),
            requested: None,
            current: None,
        }
    }

    /// `wanted` changed: request it (or clear on None). Drops a cover whose
    /// media differs from `wanted`'s.
    pub fn want(&mut self, wanted: Option<(MediaId, CoverSource)>) {
        let media = wanted.as_ref().map(|(media, _)| media);
        if self.current.as_ref().map(|(m, _)| m) != media {
            self.current = None;
        }
        if self.requested != wanted {
            if let Some((media, source)) = wanted.clone() {
                self.worker.request(media, source);
            }
            self.requested = wanted;
        }
    }

    /// Drains finished jobs; keeps a result only if its media is the
    /// requested one (Ok becomes current, Err clears it).
    pub fn poll(&mut self) {
        while let Some(result) = self.worker.try_result() {
            if self.requested.as_ref().map(|(m, _)| m) != Some(&result.media) {
                continue;
            }
            self.current = result.image.ok().map(|i| (result.media, CoverImage(i)));
        }
    }

    pub fn cover_for(&self, media: &MediaId) -> Option<CoverImage> {
        let (m, image) = self.current.as_ref()?;
        (m == media).then(|| image.clone())
    }
}

/// Wraps `inner` so the worker shrinks a decoded cover to `COVER_EDGE` at
/// once: the fetcher then keeps a small image, and the resize runs off the
/// bus thread.
pub fn shrink_loader(inner: CoverLoader) -> CoverLoader {
    Arc::new(move |source| {
        let image = inner(source)?;
        Ok(
            if image.width() > COVER_EDGE || image.height() > COVER_EDGE {
                image.thumbnail(COVER_EDGE, COVER_EDGE)
            } else {
                image
            },
        )
    })
}

/// Writes `cover-<pid>-<seq>.png`: the pid keeps URLs distinct between runs
/// and keeps two instances sharing a cache folder apart.
pub fn write_cover(dir: &Path, seq: u64, image: &DynamicImage) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let small;
    let image = if image.width() > COVER_EDGE || image.height() > COVER_EDGE {
        small = image.thumbnail(COVER_EDGE, COVER_EDGE);
        &small
    } else {
        image
    };
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io::Error::other)?;
    let pid = std::process::id();
    let path = dir.join(format!("cover-{pid}-{seq}.png"));
    let tmp = dir.join(format!("cover-{pid}-{seq}.png.tmp"));
    std::fs::write(&tmp, png.into_inner())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Removes cover files left by earlier runs: ours, and those of processes
/// that no longer exist. A live process's files stay. Names without a pid
/// (the pre-pid format) go too.
pub fn clear_covers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let own = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("cover-") else {
            continue;
        };
        if !(name.ends_with(".png") || name.ends_with(".png.tmp")) {
            continue;
        }
        let pid = rest.split('-').next().and_then(|p| p.parse::<u32>().ok());
        if pid.is_none_or(|pid| pid == own || !process_alive(pid)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Off Linux there is no `/proc`: "can't tell" counts as not alive.
fn process_alive(pid: u32) -> bool {
    cfg!(target_os = "linux") && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;
    use std::time::{Duration, Instant};

    use crate::media::id::AbsolutePath;

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::new_rgb8(w, h)
    }

    #[test]
    fn cover_images_compare_by_pointer() {
        let a = CoverImage(Arc::new(img(2, 2)));
        let b = CoverImage(Arc::new(img(2, 2)));
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn write_cover_caps_the_long_edge_and_keeps_aspect() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 1, &img(2000, 1000)).unwrap();
        let read = image::open(&path).unwrap();
        assert_eq!((read.width(), read.height()), (512, 256));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn write_cover_keeps_a_small_image_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 2, &img(100, 80)).unwrap();
        let read = image::open(path).unwrap();
        assert_eq!((read.width(), read.height()), (100, 80));
    }

    #[test]
    fn clear_covers_removes_only_cover_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["cover-3.png", "cover-4.png.tmp", "keep.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        clear_covers(dir.path());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["keep.txt"]);
    }

    #[test]
    fn cover_files_carry_the_pid_so_urls_differ_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cover(dir.path(), 1, &img(4, 4)).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("cover-{}-1.png", std::process::id()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clear_covers_spares_a_live_process_and_removes_dead_and_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let own = std::process::id();
        // Pid 1 is alive; u32::MAX is never a pid.
        let names = [
            "cover-1-1.png".to_string(),
            "cover-1-2.png.tmp".to_string(),
            format!("cover-{}-1.png", u32::MAX),
            format!("cover-{own}-7.png"),
            "keep.txt".to_string(),
        ];
        for name in &names {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        clear_covers(dir.path());
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["cover-1-1.png", "cover-1-2.png.tmp", "keep.txt"]);
    }

    #[test]
    fn the_shrinking_loader_caps_the_long_edge() {
        let big: CoverLoader = Arc::new(|_| Ok(img(3000, 2000)));
        let small: CoverLoader = Arc::new(|_| Ok(img(100, 80)));
        let source = local("x").1;
        let out = shrink_loader(big)(&source).unwrap();
        assert_eq!((out.width(), out.height()), (512, 341));
        let out = shrink_loader(small)(&source).unwrap();
        assert_eq!((out.width(), out.height()), (100, 80));
    }

    fn local(name: &str) -> (MediaId, CoverSource) {
        let path = AbsolutePath::new(format!("/{name}").into()).unwrap();
        (MediaId::LocalFile(path.clone()), CoverSource::Local(path))
    }

    #[test]
    fn a_stale_result_never_becomes_the_cover() {
        let (release_a, held) = crossbeam_channel::bounded::<()>(0);
        let (started_tx, started) = crossbeam_channel::bounded::<()>(1);
        let loader: CoverLoader = Arc::new(move |source| match source {
            CoverSource::Local(p) if p.as_str().ends_with("/a") => {
                let _ = started_tx.send(());
                let _ = held.recv();
                Ok(img(1, 1))
            }
            _ => Ok(img(2, 2)),
        });
        let mut fetcher = CoverFetcher::new(loader);
        let (a, a_source) = local("a");
        let (b, b_source) = local("b");
        fetcher.want(Some((a.clone(), a_source)));
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("A's job never started"); // A's job is now in flight, blocked.
        fetcher.want(Some((b.clone(), b_source)));
        drop(release_a);
        let deadline = Instant::now() + Duration::from_secs(10);
        while fetcher.cover_for(&b).is_none() {
            assert!(Instant::now() < deadline, "B's cover never arrived");
            fetcher.poll();
            assert!(fetcher.cover_for(&a).is_none(), "A's cover leaked");
            std::thread::yield_now();
        }
        let image = fetcher.cover_for(&b).unwrap();
        assert_eq!((image.0.width(), image.0.height()), (2, 2));
        assert!(fetcher.cover_for(&a).is_none());
    }
}
