//! A single background worker that resolves and decodes cover art (design
//! doc M5 §9, §11, decision 24): one thread, one latest-wins request slot,
//! and every job run inside [`run_contained`] so a decoder panic becomes an
//! ordinary [`ArtworkError::Panicked`] result instead of taking the process
//! down. A local track's cover is read from its tags or a sibling file; a
//! podcast episode's is downloaded from the feed's `itunes:image` URL
//! through the HTTP service playback already opened; failing that, and for
//! a plain remote URL, the front cover the decoder found in the stream's
//! own tag once it was loaded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::artwork::decode::{ArtworkError, decode_limited, read_limited};
use crate::artwork::resolve::{ArtworkSource, find_artwork};
use crate::http::document::{DocumentOutcome, DocumentRequest};
use crate::http::service::HttpService;
use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::panic::run_contained;
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::tags::{CoverBytes, probe_local_tags};
use url::Url;

/// Where a cover comes from.
#[derive(Clone)]
pub enum CoverSource {
    Local(AbsolutePath),
    Remote {
        url: Url,
        http: Arc<HttpService>,
    },
    /// A front cover the playback decoder already read from the loaded
    /// stream's own tag (ID3 `APIC`, FLAC `PICTURE`, MP4 `covr`).
    Embedded(Arc<CoverBytes>),
}

/// Hand-written because `Arc<HttpService>` is not `PartialEq`, so `derive`
/// cannot see past it: `Remote` compares by URL alone, ignoring which
/// `HttpService` handle carried it, and `Embedded` compares by `Arc::ptr_eq`
/// so a re-delivered identical cover still dedups. This is the key
/// `Covers::requested` (`src/application/cover.rs`) widens to, so a re-probed
/// station's changed logo URL is a different source even though its media
/// identity is not (M7.1 §8.1).
impl PartialEq for CoverSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => a == b,
            (Self::Remote { url: a, .. }, Self::Remote { url: b, .. }) => a == b,
            (Self::Embedded(a), Self::Embedded(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Loads the decoded cover for a source. Shared with the worker thread, so
/// it must be callable from another thread.
pub type CoverLoader =
    Arc<dyn Fn(&CoverSource) -> Result<image::DynamicImage, ArtworkError> + Send + Sync>;

pub struct ArtworkResult {
    pub media: MediaId,
    pub image: Result<Arc<image::DynamicImage>, ArtworkError>,
}

struct Job {
    media: MediaId,
    source: CoverSource,
}

/// A single artwork worker thread with a latest-wins request slot: a new
/// request replaces whatever request is still waiting (not yet picked up by
/// the worker) rather than queuing behind it, and [`request`](Self::request)
/// never blocks the caller to make that happen.
pub struct ArtworkWorker {
    requests: Sender<Job>,
    /// A second handle onto the request channel, used only to drain a
    /// stale pending job from [`request`](Self::request); the worker thread
    /// holds its own clone to receive from.
    request_drain: Receiver<Job>,
    results: Receiver<ArtworkResult>,
}

impl ArtworkWorker {
    /// Starts the `tenuto-artwork` thread. The thread is detached, never
    /// joined: dropping this handle drops the request sender, which ends
    /// the worker's loop once whatever job it holds (if any) returns.
    pub fn spawn(loader: CoverLoader) -> Self {
        let (requests_tx, requests_rx) = crossbeam_channel::bounded::<Job>(1);
        let (results_tx, results_rx) = crossbeam_channel::bounded::<ArtworkResult>(1);
        let worker_requests = requests_rx.clone();
        let spawned = thread::Builder::new()
            .name("tenuto-artwork".to_string())
            .spawn(move || {
                for job in &worker_requests {
                    let outcome = match run_contained("artwork", || (loader)(&job.source)) {
                        Ok(loaded) => {
                            tracing::debug!("artwork job completed");
                            loaded.map(Arc::new)
                        }
                        Err(_) => Err(ArtworkError::Panicked),
                    };
                    let result = ArtworkResult {
                        media: job.media,
                        image: outcome,
                    };
                    if results_tx.send(result).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start the artwork worker");
        }
        Self {
            requests: requests_tx,
            request_drain: requests_rx,
            results: results_rx,
        }
    }

    /// Requests a cover for `media` from `source`, replacing a request the worker
    /// has not yet picked up. Never blocks: every step is a non-blocking
    /// channel operation, and the loop below only ever repeats to retry a
    /// send that raced the worker taking the previous job, which converges
    /// in at most a couple of iterations.
    pub fn request(&self, media: MediaId, source: CoverSource) {
        let mut job = Job { media, source };
        loop {
            match self.requests.try_send(job) {
                Ok(()) => return,
                Err(TrySendError::Full(returned)) => {
                    job = returned;
                    let _ = self.request_drain.try_recv();
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }

    /// The most recently finished result, if any is waiting.
    pub fn try_result(&self) -> Option<ArtworkResult> {
        self.results.try_recv().ok()
    }
}

/// The production loader. A local track: probes its tags, resolves its
/// artwork (embedded cover first, then siblings), and decodes it within the
/// fixed limits. A remote cover: one whole-document GET through the given
/// service (its own timeouts and 8 MiB body cap apply, every failure is
/// [`ArtworkError::Remote`]), then the same bounded decode. An embedded cover the tag probe already judged oversized
/// (decision: `LocalTags::cover_oversized`) is reported as
/// [`ArtworkError::TooLarge`] directly — it outranks any sibling, so a
/// fallback would show the wrong cover rather than a placeholder for the
/// right one. Under the `artwork-job-panic` test hook, the first call
/// panics first, inside the job, so the panic is contained.
pub fn default_loader(hook: TestHook) -> CoverLoader {
    let armed = AtomicBool::new(hook == TestHook::ArtworkJobPanic);
    Arc::new(move |source| {
        if armed.swap(false, Ordering::SeqCst) {
            hook.panic_at(TestHook::ArtworkJobPanic);
        }
        let path = match source {
            CoverSource::Local(path) => path,
            CoverSource::Embedded(cover) => return decode_limited(&cover.data),
            CoverSource::Remote { url, http } => {
                let request = DocumentRequest {
                    origin: url.clone(),
                    validators: None,
                };
                let outcome = http
                    .handle()
                    .block_on(http.fetch_document(request))
                    .map_err(|_| ArtworkError::Remote)?;
                return match outcome {
                    DocumentOutcome::Fetched { bytes, .. } => decode_limited(&bytes),
                    DocumentOutcome::Unchanged { .. } => Err(ArtworkError::Remote),
                };
            }
        };
        let tags = probe_local_tags(path).map_err(|_| ArtworkError::Missing)?;
        if tags.cover_oversized {
            return Err(ArtworkError::TooLarge);
        }
        let source = find_artwork(path.as_path(), tags.front_cover).ok_or(ArtworkError::Missing)?;
        let bytes = match source {
            ArtworkSource::Embedded(data) => data,
            ArtworkSource::Sibling(sibling) => read_limited(&sibling)?,
        };
        decode_limited(&bytes)
    })
}
