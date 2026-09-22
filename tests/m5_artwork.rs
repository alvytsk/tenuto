//! Design doc M5 §9: local artwork lookup, bounded decode and a contained
//! worker.

mod support;

#[path = "support/runtime.rs"]
mod runtime;

#[path = "support/tagged_flac.rs"]
mod tagged_flac;

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tenuto::artwork::decode::{ArtworkError, MAX_ENCODED_BYTES, decode_limited, read_limited};
use tenuto::artwork::resolve::{ArtworkSource, find_artwork};
use tenuto::artwork::worker::{ArtworkWorker, CoverLoader, CoverSource, default_loader};
use tenuto::lifecycle::hooks::TestHook;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::tags::{CoverBytes, MAX_EMBEDDED_COVER_BYTES};

fn encoded(format: image::ImageFormat, w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([10, 20, 30])))
        .write_to(&mut Cursor::new(&mut bytes), format)
        .unwrap_or_else(|error| panic!("encode: {error}"));
    bytes
}

#[test]
fn embedded_art_wins_then_siblings_in_their_documented_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let track = dir.path().join("track.flac");
    for name in ["folder.png", "folder.jpg", "cover.png"] {
        std::fs::write(
            dir.path().join(name),
            encoded(image::ImageFormat::Png, 2, 2),
        )
        .expect("write");
    }
    let embedded = CoverBytes {
        data: vec![1, 2, 3],
        media_type: None,
    };
    assert_eq!(
        find_artwork(&track, Some(embedded)),
        Some(ArtworkSource::Embedded(vec![1, 2, 3]))
    );
    assert_eq!(
        find_artwork(&track, None),
        Some(ArtworkSource::Sibling(dir.path().join("cover.png")))
    );
    std::fs::write(
        dir.path().join("cover.jpg"),
        encoded(image::ImageFormat::Jpeg, 2, 2),
    )
    .expect("write");
    assert_eq!(
        find_artwork(&track, None),
        Some(ArtworkSource::Sibling(dir.path().join("cover.jpg")))
    );
    std::fs::remove_file(dir.path().join("cover.jpg")).expect("rm");
    std::fs::remove_file(dir.path().join("cover.png")).expect("rm");
    assert_eq!(
        find_artwork(&track, None),
        Some(ArtworkSource::Sibling(dir.path().join("folder.jpg")))
    );
}

#[test]
fn missing_art_is_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("cover.jpg")).expect("a directory is not artwork");
    assert_eq!(find_artwork(&dir.path().join("t.flac"), None), None);
}

#[test]
fn supported_formats_decode_and_everything_else_is_bounded() {
    assert!(decode_limited(&encoded(image::ImageFormat::Png, 3, 2)).is_ok());
    assert!(decode_limited(&encoded(image::ImageFormat::Jpeg, 3, 2)).is_ok());
    assert_eq!(
        decode_limited(b"\x89PNG\r\n\x1a\ngarbage").err(),
        Some(ArtworkError::Corrupt)
    );
    assert_eq!(
        decode_limited(b"GIF89a\x01\x00\x01\x00").err(),
        Some(ArtworkError::Unsupported)
    );

    // A valid PNG signature and IHDR declaring 5000×4000, with no pixel
    // data. Ruling 1: with image 0.25.10 / png 0.18.1, `into_dimensions()`
    // needs to see at least one IDAT chunk before it can report the
    // dimensions the IHDR already declared — without one it hits
    // `UnexpectedEof`, which this crate's rule maps to `Corrupt` rather
    // than `TooManyPixels`. A zero-length IDAT satisfies that without
    // providing any pixel data, so the dimension check still runs before
    // any decoding would.
    let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&5000u32.to_be_bytes());
    ihdr.extend_from_slice(&4000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    huge.extend_from_slice(&13u32.to_be_bytes());
    huge.extend_from_slice(&ihdr);
    huge.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    huge.extend_from_slice(&0u32.to_be_bytes());
    huge.extend_from_slice(b"IDAT");
    huge.extend_from_slice(&crc32(b"IDAT").to_be_bytes());
    assert_eq!(
        decode_limited(&huge).err(),
        Some(ArtworkError::TooManyPixels)
    );
}

/// PNG chunk CRC (ISO 3309), so the header parses as valid.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn an_oversized_file_is_refused_before_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cover.jpg");
    std::fs::File::create(&path)
        .expect("create")
        .set_len(MAX_ENCODED_BYTES + 1)
        .expect("sparse");
    assert_eq!(read_limited(&path).err(), Some(ArtworkError::TooLarge));
}

#[test]
fn a_panicking_decode_is_a_placeholder_and_the_next_job_succeeds() {
    let first = Arc::new(AtomicBool::new(true));
    let flag = first.clone();
    let loader: CoverLoader = Arc::new(move |_| {
        if flag.swap(false, Ordering::SeqCst) {
            panic!("injected artwork panic");
        }
        Ok(image::DynamicImage::new_rgb8(1, 1))
    });
    let worker = ArtworkWorker::spawn(loader);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    let media = MediaId::LocalFile(path.clone());
    let wait = |worker: &ArtworkWorker| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = worker.try_result() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    worker.request(media.clone(), CoverSource::Local(path.clone()));
    assert_eq!(wait(&worker).image.err(), Some(ArtworkError::Panicked));
    worker.request(media, CoverSource::Local(path));
    assert!(wait(&worker).image.is_ok());
}

/// Ruling 3: an embedded front cover the tag probe already judged too large
/// (`LocalTags::cover_oversized`) outranks any sibling file, so it produces
/// a placeholder (`ArtworkError::TooLarge`) rather than silently falling
/// back to a sibling cover next to the track.
#[test]
fn an_oversized_embedded_cover_is_a_placeholder_and_never_falls_back_to_a_sibling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let oversized = vec![0u8; MAX_EMBEDDED_COVER_BYTES + 1];
    let track = tagged_flac::tagged_flac(dir.path(), "Title", "Artist", "Album", Some(&oversized));
    std::fs::write(
        dir.path().join("cover.jpg"),
        encoded(image::ImageFormat::Jpeg, 2, 2),
    )
    .expect("write sibling");

    let path = AbsolutePath::new(track).expect("abs");
    let loader = default_loader(TestHook::None);
    assert_eq!(
        loader(&CoverSource::Local(path)).err(),
        Some(ArtworkError::TooLarge)
    );
}

/// A remote cover is fetched through the HTTP service the player already
/// opened for playback, then decoded under the same limits as a local one;
/// a failed fetch is `Remote`, not a panic and not a stray placeholder-less
/// error.
#[test]
fn a_remote_cover_is_fetched_and_decoded_and_a_failed_fetch_is_reported() {
    use support::server::{Script, TestServer};
    use tenuto::http::limits::Limits;
    use tenuto::http::service::HttpService;

    let http = HttpService::spawn(Limits::brisk()).expect("http service");
    let loader = default_loader(TestHook::None);

    let served = TestServer::start(Script::serving(encoded(image::ImageFormat::Png, 3, 2)));
    let url = served.url("/cover.png").parse().expect("url");
    let source = CoverSource::Remote {
        url,
        http: Arc::clone(&http),
    };
    let image = loader(&source).expect("remote cover decodes");
    assert_eq!((image.width(), image.height()), (3, 2));
    served.shutdown();

    let missing = TestServer::start(Script::serving(Vec::new()).status(404));
    let url = missing.url("/cover.png").parse().expect("url");
    let source = CoverSource::Remote { url, http };
    assert_eq!(loader(&source).err(), Some(ArtworkError::Remote));
    missing.shutdown();
}

/// A remote stream whose tag carries a front cover (ID3 `APIC`, FLAC
/// `PICTURE`): once the track is loaded, the decoder's copy of that cover
/// becomes the active entry's cover source, with no second request.
#[test]
fn a_remote_streams_embedded_front_cover_becomes_the_active_cover_source() {
    use runtime::{rig_with, row_ids};
    use support::server::{Script, TestServer};
    use tenuto::application::runtime::AppCommand;
    use tenuto::media::id::NormalizedUrl;
    use tenuto::persistence::model::PersistedState;
    use tenuto::queue::{NewQueueEntry, QueueSource};
    use tenuto::session::Session;

    let dir = tempfile::tempdir().expect("tempdir");
    let track = tagged_flac::tagged_flac(
        dir.path(),
        "Title",
        "Artist",
        "Album",
        Some(&encoded(image::ImageFormat::Png, 4, 3)),
    );
    let server = TestServer::start(Script::serving(std::fs::read(track).expect("read track")));
    let url = NormalizedUrl::parse(&server.url("/a.flac")).expect("url");
    let entry = NewQueueEntry::new(
        MediaId::RemoteUrl(url.clone()),
        QueueSource::RemoteUrl(url),
        Default::default(),
    )
    .expect("entry");
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(session.state().playing(), vec![entry])
        .expect("fits");
    let mut rig = rig_with(session.state().clone());

    rig.runtime.pump();
    assert!(rig.runtime.active_cover().is_none(), "nothing before play");
    let key_before = rig.runtime.cover_key();
    let remote = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(remote));
    let deadline = Instant::now() + Duration::from_secs(10);
    let source = loop {
        rig.runtime.pump();
        if let Some((_, source)) = rig.runtime.active_cover() {
            break source;
        }
        assert!(
            Instant::now() < deadline,
            "no cover source: {:?}",
            rig.runtime.view()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(matches!(source, CoverSource::Embedded(_)));
    // The media did not change, but the cover became available: the cheap
    // key the player polls must reflect that, or the placeholder would stay.
    assert_ne!(
        rig.runtime.cover_key(),
        key_before,
        "loading made the cover available without changing the media"
    );
    let requests_before = server.requests().len();
    let image = default_loader(TestHook::None)(&source).expect("embedded cover decodes");
    assert_eq!((image.width(), image.height()), (4, 3));
    assert_eq!(
        server.requests().len(),
        requests_before,
        "the cover cost no extra request"
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}
