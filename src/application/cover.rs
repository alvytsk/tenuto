//! The playing media's cover, resolved and decoded once for every front end:
//! the TUI's cover area and the desktop media controls' `artUrl` both read
//! [`Covers::shown`], so a remote cover is fetched once per track, on one
//! `tenuto-artwork` worker. The cover of the entry being loaded is fetched
//! ahead, so it is ready when `Loaded` makes that entry the active one.

use std::sync::Arc;

use image::DynamicImage;

use crate::artwork::decode::ArtworkError;
use crate::artwork::worker::{ArtworkWorker, CoverLoader, CoverSource};
use crate::media::id::MediaId;

/// The cover for the playing media. `image` is `None` while it is on its way
/// and after it failed.
#[derive(Clone)]
pub struct Shown {
    pub media: MediaId,
    pub image: Option<Arc<DynamicImage>>,
}

/// Same media and the same decoded image, by pointer: a front end can tell a
/// new cover from the one it already has without comparing pixels.
impl PartialEq for Shown {
    fn eq(&self, other: &Self) -> bool {
        self.media == other.media
            && match (&self.image, &other.image) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

struct Ahead {
    media: MediaId,
    source: CoverSource,
    image: Option<Arc<DynamicImage>>,
}

pub struct Covers {
    loader: CoverLoader,
    /// Started on the first request.
    worker: Option<ArtworkWorker>,
    /// The media whose cover was last requested, **and the source it was
    /// requested from**: a re-probe can change a station's (or a feed's)
    /// artwork URL without changing its media identity (M7.1 §8.1).
    requested: Option<(MediaId, CoverSource)>,
    shown: Option<Shown>,
    /// One slot: a decoded cover can run to tens of megabytes.
    ahead: Option<Ahead>,
    failure: Option<ArtworkError>,
}

impl Covers {
    pub fn new(loader: CoverLoader) -> Self {
        Self {
            loader,
            worker: None,
            requested: None,
            shown: None,
            ahead: None,
            failure: None,
        }
    }

    /// The playing media's cover source changed: requests it, unless it was
    /// fetched ahead. `None` clears the cover.
    pub fn want(&mut self, wanted: Option<(MediaId, CoverSource)>) {
        if self.requested == wanted {
            return;
        }
        self.shown = None;
        let Some((media, source)) = wanted.clone() else {
            self.requested = None;
            return;
        };
        let ahead = self
            .ahead
            .take()
            .filter(|ahead| ahead.media == media && ahead.source == source);
        // Fetched ahead: shown at once, or still on its way and shown by
        // `poll` when it lands.
        let image = ahead.as_ref().and_then(|ahead| ahead.image.clone());
        if ahead.is_none() {
            self.request(media.clone(), source);
        }
        self.shown = Some(Shown { media, image });
        self.requested = wanted;
    }

    /// The entry being loaded changed: fetches its cover ahead, or drops the
    /// cover fetched for the previous one.
    pub fn want_ahead(&mut self, ahead: Option<(MediaId, CoverSource)>) {
        self.ahead = ahead.map(|(media, source)| {
            self.request(media.clone(), source.clone());
            Ahead {
                media,
                source,
                image: None,
            }
        });
    }

    /// Takes finished jobs: the requested media's becomes the shown cover,
    /// the ahead one is kept for `want`, anything else is stale.
    pub fn poll(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        while let Some(result) = worker.try_result() {
            if self.requested.as_ref().map(|(media, _)| media) != Some(&result.media) {
                if let Some(ahead) = &mut self.ahead
                    && ahead.media == result.media
                {
                    match result.image {
                        Ok(image) => ahead.image = Some(image),
                        // Requested again once active, to report the failure.
                        Err(_) => self.ahead = None,
                    }
                }
                continue;
            }
            let image = match result.image {
                Ok(image) => Some(image),
                Err(error) => {
                    tracing::debug!(%error, "no cover art for the active entry");
                    self.failure = Some(error);
                    None
                }
            };
            self.shown = Some(Shown {
                media: result.media,
                image,
            });
        }
    }

    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    /// The last failure to load the shown cover, once.
    pub fn take_failure(&mut self) -> Option<ArtworkError> {
        self.failure.take()
    }

    fn request(&mut self, media: MediaId, source: CoverSource) {
        let loader = &self.loader;
        self.worker
            .get_or_insert_with(|| ArtworkWorker::spawn(Arc::clone(loader)))
            .request(media, source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::media::id::AbsolutePath;

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::new_rgb8(w, h)
    }

    fn local(name: &str) -> (MediaId, CoverSource) {
        let path = AbsolutePath::new(format!("/{name}").into()).unwrap();
        (MediaId::LocalFile(path.clone()), CoverSource::Local(path))
    }

    fn image_of<'a>(covers: &'a Covers, media: &MediaId) -> Option<&'a Arc<DynamicImage>> {
        covers
            .shown()
            .filter(|shown| &shown.media == media)?
            .image
            .as_ref()
    }

    fn poll_until(covers: &mut Covers, what: &str, done: impl Fn(&Covers) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(covers) {
            assert!(Instant::now() < deadline, "{what} never happened");
            covers.poll();
            std::thread::yield_now();
        }
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
        let mut covers = Covers::new(loader);
        let (a, a_source) = local("a");
        let (b, b_source) = local("b");
        covers.want(Some((a.clone(), a_source)));
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("A's job never started"); // A's job is now in flight, blocked.
        covers.want(Some((b.clone(), b_source)));
        drop(release_a);
        poll_until(&mut covers, "B's cover", |covers| {
            assert!(image_of(covers, &a).is_none(), "A's cover leaked");
            image_of(covers, &b).is_some()
        });
        let image = image_of(&covers, &b).unwrap();
        assert_eq!((image.width(), image.height()), (2, 2));
    }

    #[test]
    fn a_cover_fetched_ahead_is_shown_without_a_second_load() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&loads);
        let loader: CoverLoader = Arc::new(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(img(3, 3))
        });
        let mut covers = Covers::new(loader);
        let (b, b_source) = local("b");
        covers.want_ahead(Some((b.clone(), b_source.clone())));
        poll_until(&mut covers, "the ahead cover", |covers| {
            covers.ahead.as_ref().is_some_and(|a| a.image.is_some())
        });
        covers.want(Some((b.clone(), b_source)));
        assert!(image_of(&covers, &b).is_some(), "shown at once");
        assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_load_is_reported_once_and_shows_no_image() {
        let loader: CoverLoader = Arc::new(|_| Err(ArtworkError::Corrupt));
        let mut covers = Covers::new(loader);
        let (a, a_source) = local("a");
        covers.want(Some((a.clone(), a_source)));
        poll_until(&mut covers, "the failure", |covers| {
            covers.failure.is_some()
        });
        assert!(image_of(&covers, &a).is_none());
        assert!(matches!(covers.take_failure(), Some(ArtworkError::Corrupt)));
        assert!(covers.take_failure().is_none());
    }
}
