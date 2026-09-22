//! Background metadata enrichment for local queue entries (design doc M5
//! §8, §11): a small, fixed pool of worker threads runs the tag probe on
//! files the queue knows only by name and hands back what it found.
//!
//! Each probe is a disposable job inside [`run_contained`], so a decoder
//! panic becomes an ordinary [`EnrichOutcome::Panicked`] and the worker
//! takes the next job. Nothing here touches `Session`, the terminal or the
//! network: the runtime applies results through `Session` on its own
//! thread, and only local paths can be requested at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender};

use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::panic::run_contained;
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::tags::{LocalTags, probe_local_tags};
use crate::playback::error::PlaybackError;

/// Queued jobs beyond the ones in hand; one per entry the playlists can
/// hold, so a folder add of a whole library — or the re-request `vacate`
/// makes after `cancel_all` — never silently drops a probe (M8 §5).
const REQUEST_CAPACITY: usize = crate::queue::MAX_PLAYLIST_ENTRIES;
/// Finished results waiting for the runtime; a runtime that stops draining
/// holds a handful rather than a queue's worth, and the workers wait
/// instead. Results carry no cover bytes (see [`Worker::serve`]).
const RESULT_CAPACITY: usize = 16;

/// Reads a local file's tags. Shared by every worker, so it must be callable
/// from several threads at once.
pub type TagProbe = Arc<dyn Fn(&AbsolutePath) -> Result<LocalTags, PlaybackError> + Send + Sync>;

#[derive(Clone, Debug)]
pub enum EnrichOutcome {
    Tags(LocalTags),
    /// The probe returned an error, as displayable text.
    Failed(String),
    /// The probe panicked; the panic was contained and its state discarded.
    Panicked,
}

#[derive(Clone, Debug)]
pub struct EnrichResult {
    pub media: MediaId,
    /// The [`MetadataWorkers::cancel_all`] generation the request was made
    /// in. [`MetadataWorkers::try_result`] only returns current ones.
    pub generation: u64,
    pub outcome: EnrichOutcome,
}

struct Job {
    media: MediaId,
    path: AbsolutePath,
    generation: u64,
}

pub struct MetadataWorkers {
    requests: Sender<Job>,
    /// The workers' own end of the request channel, kept so `cancel_all`
    /// can empty it.
    backlog: Receiver<Job>,
    results: Receiver<EnrichResult>,
    generation: Arc<AtomicU64>,
}

impl MetadataWorkers {
    /// Starts `workers` threads named `tenuto-metadata-1` onwards.
    ///
    /// The threads are detached, never joined: a probe stuck on a slow or
    /// hung mount must not hold up whoever drops this handle, least of all
    /// runtime shutdown flushing state. Dropping the handle cancels whatever
    /// is queued and closes the channel, so each thread exits once the job in
    /// hand (if any) returns, and that job's result has nowhere to go.
    pub fn spawn(workers: usize, probe: TagProbe, hook: TestHook) -> Self {
        let (requests, backlog) = crossbeam_channel::bounded(REQUEST_CAPACITY);
        let (result_tx, results) = crossbeam_channel::bounded(RESULT_CAPACITY);
        let generation = Arc::new(AtomicU64::new(0));
        for index in 1..=workers {
            let worker = Worker {
                requests: backlog.clone(),
                results: result_tx.clone(),
                probe: Arc::clone(&probe),
                generation: Arc::clone(&generation),
                hook,
            };
            let spawned = thread::Builder::new()
                .name(format!("tenuto-metadata-{index}"))
                .spawn(move || worker.serve());
            if let Err(error) = spawned {
                tracing::warn!(%error, "cannot start a metadata worker");
            }
        }
        Self {
            requests,
            backlog,
            results,
            generation,
        }
    }

    /// Queues a probe of `path` on behalf of `media`; never blocks. A full
    /// backlog drops the request: the entry keeps its fallback name.
    pub fn request(&self, media: MediaId, path: AbsolutePath) {
        let job = Job {
            media,
            path,
            generation: self.generation.load(Ordering::SeqCst),
        };
        // Never disconnected: this handle holds a receiver too.
        if self.requests.try_send(job).is_err() {
            tracing::debug!("metadata request dropped: the backlog is full");
        }
    }

    /// Discards every queued request and every result of a request made
    /// before this call. A job already running finishes, but its result is
    /// never returned.
    pub fn cancel_all(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        while self.backlog.try_recv().is_ok() {}
    }

    /// The next result of a request made since the last `cancel_all`.
    pub fn try_result(&self) -> Option<EnrichResult> {
        let current = self.generation.load(Ordering::SeqCst);
        loop {
            let result = self.results.try_recv().ok()?;
            if result.generation == current {
                return Some(result);
            }
        }
    }
}

impl Drop for MetadataWorkers {
    fn drop(&mut self) {
        // The channel stays readable after its sender is gone, so without
        // this the detached workers would go on probing the whole backlog.
        self.cancel_all();
    }
}

struct Worker {
    requests: Receiver<Job>,
    results: Sender<EnrichResult>,
    probe: TagProbe,
    generation: Arc<AtomicU64>,
    hook: TestHook,
}

impl Worker {
    fn serve(self) {
        let mut first = true;
        for job in &self.requests {
            if first {
                first = false;
                // Deliberately outside the contained boundary: the
                // uncontained worker panic a process test provokes. Fired on
                // the first job rather than at thread start so it happens
                // only once the front end is running and has asked for work.
                self.hook.panic_at(TestHook::WorkerPanic);
            }
            if job.generation != self.generation.load(Ordering::SeqCst) {
                continue;
            }
            let outcome = match run_contained("metadata", || (self.probe)(&job.path)) {
                Ok(probed) => {
                    tracing::debug!("metadata job completed");
                    match probed {
                        // Enrichment only fills text and duration; artwork
                        // has its own loader. Dropped here, inside the
                        // worker, so a queued result never holds up to
                        // 10 MiB of cover it will not use.
                        Ok(tags) => EnrichOutcome::Tags(LocalTags {
                            front_cover: None,
                            ..tags
                        }),
                        Err(error) => EnrichOutcome::Failed(error.to_string()),
                    }
                }
                Err(_) => EnrichOutcome::Panicked,
            };
            let result = EnrichResult {
                media: job.media,
                generation: job.generation,
                outcome,
            };
            if self.results.send(result).is_err() {
                return;
            }
        }
    }
}

/// The production probe: [`probe_local_tags`], except that under the
/// `metadata-job-panic` test hook its first call panics first — inside the
/// job, so the panic is contained.
pub fn default_probe(hook: TestHook) -> TagProbe {
    let armed = AtomicBool::new(hook == TestHook::MetadataJobPanic);
    Arc::new(move |path| {
        if armed.swap(false, Ordering::SeqCst) {
            hook.panic_at(TestHook::MetadataJobPanic);
        }
        probe_local_tags(path)
    })
}
