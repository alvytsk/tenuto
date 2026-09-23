//! Cover art in the terminal (design doc M5 §9, §11, decision 24): which
//! image protocol the terminal speaks, the one prepared cover the frame
//! draws, and the contained boundary its resize and encoding run in.
//!
//! Detection happens once per run. Preparation happens in the loop before
//! `terminal.draw`, never inside it, and only when what the cover depends on
//! — media, artwork mode or cover area — changed. Only building the
//! disposable candidate runs inside [`prepare_contained`]; installing it,
//! clearing it and requesting placement cleanup happen outside, so a panic
//! can discard the candidate but never leave the cache half-updated.

use std::sync::Arc;
use std::time::Duration;

use image::DynamicImage;
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::picker::Picker;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use crate::artwork::decode::ArtworkError;
use crate::artwork::default::{CoverKind, default_cover};
use crate::cli::ArtworkMode;
use crate::lifecycle::hooks::TestHook;
use crate::media::id::MediaId;
use crate::tui::render::CoverWidget;

/// How long `--artwork auto` waits for the terminal to answer its
/// capability query before settling for half-blocks.
pub const DETECTION_TIMEOUT: Duration = Duration::from_millis(250);

/// Runs one resize-and-encode job inside the contained-panic boundary
/// (§11), so an unwinding panic becomes [`ArtworkError::Panicked`] instead
/// of taking the TUI down.
pub fn prepare_contained<T>(
    job: impl FnOnce() -> Result<T, ArtworkError>,
) -> Result<T, ArtworkError> {
    crate::lifecycle::panic::run_contained("artwork encoding", job)
        .map_err(|_| ArtworkError::Panicked)?
}

/// The picker `mode` calls for: none for `off`, half-blocks for `blocks`
/// without asking the terminal anything, and for `auto` whatever `query`
/// detects within [`DETECTION_TIMEOUT`], or half-blocks when it detects
/// nothing.
pub fn picker_for(
    mode: ArtworkMode,
    query: impl FnOnce(Duration) -> Option<Picker>,
) -> Option<Picker> {
    match mode {
        ArtworkMode::Off => None,
        ArtworkMode::Blocks => Some(Picker::halfblocks()),
        ArtworkMode::Auto => Some(query(DETECTION_TIMEOUT).unwrap_or_else(Picker::halfblocks)),
    }
}

/// How long the library's capability query may take once the terminal has
/// answered the status-report probe. The library's timeout restarts with
/// every chunk of its answer.
const ANSWERED_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Asks the terminal on stdio which image protocol it supports. A terminal
/// that answers nothing within `timeout` gets `None` at once; one that
/// answered has `ANSWERED_QUERY_TIMEOUT` to answer the full query. `None`
/// too when the query failed; a picker built without any reported
/// capability is a guess, not a detection, and counts as no answer.
///
/// `Picker::from_query_stdio_with_options` reads the answer on a thread it
/// never joins, which stops only once it reads the status report that ends
/// its query. If this side gave up first, that thread would keep reading
/// stdin next to the input reader: should the input reader take the final
/// report, the thread would swallow every later key for the rest of the run,
/// and a thread that did finish after teardown would restore its raw-mode
/// terminal settings. So the query goes only to a terminal that has just
/// answered a status report (`terminal_answers`), and it gets a timeout far
/// beyond that answer's delay, so its thread has read the final report
/// before this returns. The remaining risk is a terminal that answers the
/// probe but then takes longer than `ANSWERED_QUERY_TIMEOUT` between two
/// chunks of the query's answer, or drops the query's own status report.
pub fn query_terminal(timeout: Duration) -> Option<Picker> {
    if !terminal_answers(timeout) {
        return None;
    }
    let picker = Picker::from_query_stdio_with_options(answered_query_options()).ok()?;
    (!picker.capabilities().is_empty()).then_some(picker)
}

fn answered_query_options() -> QueryStdioOptions {
    QueryStdioOptions {
        timeout: ANSWERED_QUERY_TIMEOUT,
        ..QueryStdioOptions::default()
    }
}

/// The status line for an artwork failure on the active entry (§11): `None`
/// when there simply is no artwork, otherwise the error's own description.
pub fn failure_status(error: &ArtworkError) -> Option<String> {
    match error {
        ArtworkError::Missing => None,
        error => Some(format!("Cover art unavailable: {error}")),
    }
}

/// Device Status Report, which terminal emulators answer with `ESC [ 0 n`.
const STATUS_QUERY: &[u8] = b"\x1b[5n";
/// How often [`terminal_answers`] looks for the reply.
const ANSWER_POLL: Duration = Duration::from_millis(5);

/// Whether the terminal answers a status report within `timeout`, read
/// without any thread or read that can outlive this call: the controlling
/// terminal is opened a second time, as its own open file, and read
/// non-blocking until the reply or the deadline. Std can only switch a
/// descriptor to non-blocking through a socket handle; the `FIONBIO` it
/// issues applies to any descriptor, and the handle is turned straight back
/// into a `File`. The flag belongs to this open file alone, so the input
/// reader's own descriptor stays blocking. Bytes read before the reply —
/// keys typed in the first moments — are dropped.
#[cfg(unix)]
fn terminal_answers(timeout: Duration) -> bool {
    use std::fs::{File, OpenOptions};
    use std::io::{IsTerminal, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    let Ok(tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return false;
    };
    let handle = UnixStream::from(OwnedFd::from(tty));
    if handle.set_nonblocking(true).is_err() {
        return false;
    }
    let mut tty = File::from(OwnedFd::from(handle));
    if tty
        .write_all(STATUS_QUERY)
        .and_then(|()| tty.flush())
        .is_err()
    {
        return false;
    }

    await_status_report(&mut tty, timeout)
}

/// Reads the non-blocking `tty` until a status report arrives (`true`) or
/// `timeout` passes, the input ends or a read fails (`false`).
#[cfg_attr(not(unix), allow(dead_code))]
fn await_status_report(tty: &mut impl std::io::Read, timeout: Duration) -> bool {
    use std::io::ErrorKind;
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 64];
    loop {
        match tty.read(&mut chunk) {
            Ok(0) => return false,
            Ok(read) => {
                reply.extend_from_slice(&chunk[..read]);
                if has_status_report(&reply) {
                    return true;
                }
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ANSWER_POLL);
    }
}

/// The terminal player never runs without Unix fd-2 redirection, so there is
/// nothing to ask.
#[cfg(not(unix))]
fn terminal_answers(_timeout: Duration) -> bool {
    false
}

/// Whether `bytes` holds a status report, `ESC [ <digits> n`.
fn has_status_report(bytes: &[u8]) -> bool {
    bytes.windows(2).enumerate().any(|(start, pair)| {
        pair == b"\x1b[" && {
            let rest = &bytes[start + 2..];
            let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
            digits > 0 && rest.get(digits) == Some(&b'n')
        }
    })
}

/// Everything a prepared cover depends on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverKey {
    pub media: MediaId,
    pub mode: ArtworkMode,
    pub area: Rect,
}

/// Resizes and encodes `image` for `picker`'s protocol to fit `size`.
type Encoder = fn(&Picker, DynamicImage, Size) -> Result<Protocol, ArtworkError>;

fn encode(picker: &Picker, image: DynamicImage, size: Size) -> Result<Protocol, ArtworkError> {
    picker
        .new_protocol(image, size, Resize::Fit(None))
        .map_err(|_| ArtworkError::Encoding)
}

/// A cover encoded for the terminal, drawn by `render::draw`.
struct PreparedCover(Protocol);

impl CoverWidget for PreparedCover {
    fn render_cover(&self, area: Rect, buffer: &mut Buffer) {
        Image::new(&self.0).render(area, buffer);
    }
}

/// The active entry's decoded cover and the one prepared representation of
/// it, kept until its [`CoverKey`] changes.
pub struct CoverCache {
    media: Option<MediaId>,
    image: Option<Arc<DynamicImage>>,
    /// What `prepared` was built for.
    key: Option<CoverKey>,
    prepared: Option<PreparedCover>,
    /// The last key whose preparation failed, so it is not retried on every
    /// frame; any other key, a new image or `invalidate` allows a retry.
    failed: Option<CoverKey>,
    placement_dirty: bool,
    /// Why the last preparation failed, until the loop takes it.
    failure: Option<ArtworkError>,
    /// The `artwork-encoding-panic` hook, consumed by the first preparation
    /// of real artwork — a stand-in leaves it alone (see [`Self::standin`]).
    panic_next_encoding: bool,
    /// Whether `image` is a built-in stand-in rather than the listener's own
    /// cover. A stand-in is this player's decoration, so failing to encode
    /// one reports nothing: "Cover art unavailable" would name a cover that
    /// was never missing.
    standin: bool,
    encoder: Encoder,
}

impl CoverCache {
    pub fn new(hook: TestHook) -> Self {
        Self::with_encoder(hook, encode)
    }

    fn with_encoder(hook: TestHook, encoder: Encoder) -> Self {
        Self {
            media: None,
            image: None,
            key: None,
            prepared: None,
            failed: None,
            placement_dirty: false,
            failure: None,
            panic_next_encoding: hook == TestHook::ArtworkEncodingPanic,
            standin: false,
            encoder,
        }
    }

    /// The decoded cover for `media`. When there is none, `kind`'s built-in
    /// cover stands in, so a track with no artwork shows a record, a wave or
    /// a microphone rather than the drawn placeholder; only a `kind` of
    /// `None` — nothing is playing — falls through to that. Either way it
    /// replaces what was there, and a cover already on screen is dropped and
    /// its placement cleaned up.
    pub fn set_image(
        &mut self,
        media: MediaId,
        kind: Option<CoverKind>,
        image: Option<Arc<DynamicImage>>,
    ) {
        self.media = Some(media);
        self.standin = image.is_none();
        self.image = image.or_else(|| kind.and_then(default_cover));
        self.failed = None;
        self.failure = None;
        self.drop_prepared();
    }

    /// Makes the prepared cover match `picker`, `mode` and `area`, encoding
    /// only when that key changed since the last preparation. `true` when it
    /// (re)encoded successfully. No picker (`off`), no cover area (the
    /// minimal tier, say), no image, or a failed encoding leaves nothing
    /// prepared, so the frame draws the placeholder.
    pub fn prepare(
        &mut self,
        picker: Option<&Picker>,
        mode: ArtworkMode,
        area: Option<Rect>,
    ) -> bool {
        let area = area.filter(|area| area.width > 0 && area.height > 0);
        let (Some(picker), Some(area), Some(media), Some(image)) =
            (picker, area, &self.media, &self.image)
        else {
            self.drop_prepared();
            return false;
        };
        let key = CoverKey {
            media: media.clone(),
            mode,
            area,
        };
        if self.prepared.is_some() && self.key.as_ref() == Some(&key) {
            return false;
        }
        if self.failed.as_ref() == Some(&key) {
            return false;
        }

        let image = Arc::clone(image);
        let encoder = self.encoder;
        // A stand-in leaves the hook for the real cover behind it: the hook
        // names artwork, and a stand-in is not artwork.
        let panic_now = !self.standin && std::mem::take(&mut self.panic_next_encoding);
        let candidate = prepare_contained(|| {
            if panic_now {
                TestHook::ArtworkEncodingPanic.panic_at(TestHook::ArtworkEncodingPanic);
            }
            encoder(
                picker,
                DynamicImage::clone(&image),
                Size::new(area.width, area.height),
            )
        });

        match candidate {
            Ok(protocol) => {
                tracing::debug!("artwork encoding completed");
                self.drop_prepared();
                self.prepared = Some(PreparedCover(protocol));
                self.key = Some(key);
                true
            }
            Err(error) => {
                tracing::debug!(%error, "artwork encoding failed");
                self.drop_prepared();
                self.placement_dirty = true;
                self.failed = Some(key);
                if !self.standin {
                    self.failure = Some(error);
                }
                false
            }
        }
    }

    /// Forgets the prepared cover and any failure so the next preparation
    /// encodes again, and requests placement cleanup (Ctrl-L, resize).
    pub fn invalidate(&mut self) {
        self.drop_prepared();
        self.failed = None;
        self.failure = None;
        self.placement_dirty = true;
    }

    /// `true` once after a replacement, resize or `invalidate`: the loop
    /// then clears the terminal before its next draw, so no stale image
    /// placement survives.
    pub fn take_placement_cleanup(&mut self) -> bool {
        std::mem::take(&mut self.placement_dirty)
    }

    /// Why the last preparation failed, once: a failed key is not prepared
    /// again, so each failure is reported a single time.
    pub fn take_failure(&mut self) -> Option<ArtworkError> {
        self.failure.take()
    }

    pub fn widget(&self) -> Option<&dyn CoverWidget> {
        self.prepared
            .as_ref()
            .map(|prepared| prepared as &dyn CoverWidget)
    }

    fn drop_prepared(&mut self) {
        self.key = None;
        if self.prepared.take().is_some() {
            self.placement_dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::media::id::AbsolutePath;

    /// Width of a test image whose encoding panics.
    const PANICKING_WIDTH: u32 = 13;

    static ENCODINGS: AtomicUsize = AtomicUsize::new(0);

    fn counting_encoder(
        picker: &Picker,
        image: DynamicImage,
        size: Size,
    ) -> Result<Protocol, ArtworkError> {
        ENCODINGS.fetch_add(1, Ordering::SeqCst);
        if image.width() == PANICKING_WIDTH {
            panic!("injected encoder panic");
        }
        encode(picker, image, size)
    }

    fn media(name: &str) -> MediaId {
        MediaId::LocalFile(AbsolutePath::new(format!("/music/{name}.flac").into()).unwrap())
    }

    fn square(width: u32) -> Option<Arc<DynamicImage>> {
        Some(Arc::new(DynamicImage::new_rgb8(width, width)))
    }

    /// Replays scripted reads; `None` is a read that would block.
    struct ScriptedTty(std::collections::VecDeque<Option<&'static [u8]>>);

    impl std::io::Read for ScriptedTty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                Some(Some(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(None) | None => Err(std::io::ErrorKind::WouldBlock.into()),
            }
        }
    }

    #[test]
    fn an_answering_terminal_is_recognised_across_split_and_late_reads() {
        let mut tty =
            ScriptedTty([None, Some(&b"k\x1b["[..]), None, None, Some(&b"0n"[..])].into());
        assert!(await_status_report(&mut tty, Duration::from_secs(5)));
    }

    #[test]
    fn a_silent_terminal_gives_up_at_the_deadline() {
        let started = std::time::Instant::now();
        let mut tty = ScriptedTty([Some(&b"typed keys"[..])].into());
        assert!(!await_status_report(&mut tty, Duration::from_millis(30)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "gives up promptly"
        );
    }

    #[test]
    fn the_library_query_outwaits_a_terminal_that_answered_the_probe() {
        assert!(ANSWERED_QUERY_TIMEOUT >= Duration::from_secs(2));
        assert!(ANSWERED_QUERY_TIMEOUT > DETECTION_TIMEOUT * 4);
        assert_eq!(answered_query_options().timeout, ANSWERED_QUERY_TIMEOUT);
    }

    #[test]
    fn only_real_artwork_failures_become_a_status() {
        assert_eq!(failure_status(&ArtworkError::Missing), None);
        assert_eq!(
            failure_status(&ArtworkError::Corrupt).as_deref(),
            Some("Cover art unavailable: artwork could not be decoded")
        );
        for error in [
            ArtworkError::TooLarge,
            ArtworkError::TooManyPixels,
            ArtworkError::Unsupported,
            ArtworkError::Corrupt,
            ArtworkError::Io,
            ArtworkError::Panicked,
            ArtworkError::Encoding,
        ] {
            let status = failure_status(&error).unwrap();
            assert_eq!(status, format!("Cover art unavailable: {error}"));
        }
    }

    #[test]
    fn a_status_report_is_recognised_among_other_input() {
        assert!(has_status_report(b"\x1b[0n"));
        assert!(has_status_report(b"q\x1b[A\x1b[3n"));
        assert!(!has_status_report(b"\x1b[n"));
        assert!(!has_status_report(b"\x1b[0"));
        assert!(!has_status_report(b"\x1b[12;4R"));
    }

    /// A stand-in is decoration this player chose, not artwork the listener
    /// has. So a failure encoding one says nothing — "Cover art unavailable"
    /// would name a cover that was never missing — and it leaves the real
    /// cover's encoding to meet whatever the encoder does.
    #[test]
    fn a_stand_in_encodes_silently_and_leaves_the_failure_to_real_artwork() {
        let picker = Picker::halfblocks();
        let area = Some(Rect::new(0, 0, 14, 7));
        let mut cache = CoverCache::new(TestHook::ArtworkEncodingPanic);

        cache.set_image(media("a"), Some(CoverKind::Music), None);
        assert!(
            cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
            "the stand-in encodes rather than meeting the encoding panic"
        );
        assert!(
            cache.take_failure().is_none(),
            "a stand-in has no artwork to call unavailable"
        );

        cache.set_image(media("a"), Some(CoverKind::Music), square(8));
        assert!(
            !cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
            "the real cover meets the encoding panic the stand-in left alone"
        );
        assert!(
            matches!(cache.take_failure(), Some(ArtworkError::Panicked)),
            "and that one is reported"
        );
    }

    #[test]
    fn a_panicking_replacement_clears_the_cover_once_and_the_next_key_prepares() {
        let picker = Picker::halfblocks();
        let area = Some(Rect::new(0, 0, 14, 7));
        let mut cache = CoverCache::with_encoder(TestHook::None, counting_encoder);

        cache.set_image(media("seed"), None, square(8));
        assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(cache.widget().is_some(), "seeded cover is prepared");
        assert!(
            !cache.take_placement_cleanup(),
            "a first cover replaces nothing"
        );

        cache.set_image(media("panics"), None, square(PANICKING_WIDTH));
        let before = ENCODINGS.load(Ordering::SeqCst);
        assert!(!cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(cache.widget().is_none(), "placeholder after the panic");
        assert!(!crate::lifecycle::panic::in_contained_job());
        assert!(cache.take_placement_cleanup());
        assert!(
            !cache.take_placement_cleanup(),
            "exactly one cleanup request"
        );
        assert_eq!(
            cache.take_failure(),
            Some(ArtworkError::Panicked),
            "the failure is reported"
        );
        assert!(
            !cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
            "the failed key is not retried"
        );
        assert_eq!(ENCODINGS.load(Ordering::SeqCst), before + 1);
        assert_eq!(cache.take_failure(), None, "and reported once");

        cache.set_image(media("next"), None, square(8));
        assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(cache.widget().is_some());
    }

    #[test]
    fn the_encoding_panic_hook_fires_once_inside_the_boundary() {
        let picker = Picker::halfblocks();
        let area = Some(Rect::new(0, 0, 8, 4));
        let mut cache = CoverCache::new(TestHook::ArtworkEncodingPanic);

        cache.set_image(media("first"), None, square(8));
        assert!(!cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(cache.widget().is_none());
        assert!(cache.take_placement_cleanup());

        cache.set_image(media("second"), None, square(8));
        assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(cache.widget().is_some());
    }

    #[test]
    fn off_and_hidden_areas_drop_a_prepared_cover() {
        let picker = Picker::halfblocks();
        let area = Some(Rect::new(0, 0, 14, 7));
        let mut cache = CoverCache::new(TestHook::None);
        cache.set_image(media("a"), None, square(8));
        assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));

        assert!(!cache.prepare(None, ArtworkMode::Off, area));
        assert!(cache.widget().is_none());
        assert!(cache.take_placement_cleanup());

        assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
        assert!(!cache.prepare(
            Some(&picker),
            ArtworkMode::Blocks,
            Some(Rect::new(0, 0, 0, 7))
        ));
        assert!(cache.widget().is_none());
        assert!(cache.take_placement_cleanup());
    }
}
