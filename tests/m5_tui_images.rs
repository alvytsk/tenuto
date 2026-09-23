use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use tenuto::artwork::default::CoverKind;
use tenuto::cli::ArtworkMode;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::tui::images::{CoverCache, picker_for};

#[allow(clippy::expect_used)] // A literal absolute path always parses.
fn media(name: &str) -> MediaId {
    MediaId::LocalFile(AbsolutePath::new(format!("/music/{name}.flac").into()).expect("abs"))
}

#[test]
fn off_disables_images_blocks_never_queries_and_auto_falls_back() {
    assert!(picker_for(ArtworkMode::Off, |_| panic!("off never queries")).is_none());
    assert!(picker_for(ArtworkMode::Blocks, |_| panic!("blocks never queries")).is_some());
    assert!(
        picker_for(ArtworkMode::Auto, |_| None).is_some(),
        "timeout falls back to half-blocks"
    );
}

#[test]
fn a_prepared_cover_is_reused_until_media_mode_or_area_changes() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    let area = Some(Rect::new(0, 0, 14, 7));
    cache.set_image(
        media("a"),
        None,
        Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))),
    );
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
    assert!(
        !cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
        "cached"
    );
    assert!(
        cache.prepare(
            Some(&picker),
            ArtworkMode::Blocks,
            Some(Rect::new(0, 0, 8, 4))
        ),
        "resize re-encodes"
    );
    assert!(cache.take_placement_cleanup());
    assert!(!cache.take_placement_cleanup(), "cleanup is requested once");
    cache.set_image(
        media("b"),
        None,
        Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))),
    );
    assert!(
        cache.prepare(
            Some(&picker),
            ArtworkMode::Blocks,
            Some(Rect::new(0, 0, 8, 4))
        ),
        "replacement re-encodes"
    );
    assert!(cache.take_placement_cleanup());
    cache.invalidate();
    assert!(cache.take_placement_cleanup());
    assert!(
        cache.prepare(
            Some(&picker),
            ArtworkMode::Blocks,
            Some(Rect::new(0, 0, 8, 4))
        ),
        "full redraw re-encodes"
    );
}

#[test]
fn no_image_or_no_area_renders_the_placeholder() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    cache.set_image(media("a"), None, None);
    assert!(!cache.prepare(
        Some(&picker),
        ArtworkMode::Blocks,
        Some(Rect::new(0, 0, 14, 7))
    ));
    assert!(cache.widget().is_none());
    cache.set_image(
        media("a"),
        None,
        Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))),
    );
    assert!(
        !cache.prepare(Some(&picker), ArtworkMode::Blocks, None),
        "minimal tier has no cover area"
    );
    assert!(cache.widget().is_none());
}

#[test]
fn an_encoding_panic_is_contained_and_a_later_preparation_succeeds() {
    use tenuto::artwork::decode::ArtworkError;
    use tenuto::lifecycle::panic::in_contained_job;
    use tenuto::tui::images::prepare_contained;
    let contained_inside = AtomicBool::new(false);
    let failed = prepare_contained(|| -> Result<(), ArtworkError> {
        contained_inside.store(in_contained_job(), Ordering::SeqCst);
        panic!("encoding panic");
    });
    assert_eq!(failed, Err(ArtworkError::Panicked));
    assert!(
        contained_inside.load(Ordering::SeqCst),
        "the job ran inside the contained boundary"
    );
    assert!(!in_contained_job());
    assert_eq!(prepare_contained(|| Ok(7)), Ok(7));
}

#[test]
fn a_track_with_no_artwork_stands_in_its_kind_default() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    let area = Some(Rect::new(0, 0, 14, 7));

    cache.set_image(media("a"), Some(CoverKind::Music), None);
    assert!(
        cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
        "the built-in record encodes even though the track carried no cover"
    );
    assert!(
        cache.widget().is_some(),
        "a track without artwork shows a cover, not the drawn placeholder"
    );
}

#[test]
fn a_real_cover_displaces_the_kind_default() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    let area = Some(Rect::new(0, 0, 14, 7));

    cache.set_image(media("a"), Some(CoverKind::Podcast), None);
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
    cache.set_image(
        media("a"),
        Some(CoverKind::Podcast),
        Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))),
    );
    assert!(
        cache.prepare(Some(&picker), ArtworkMode::Blocks, area),
        "the arriving cover re-encodes over the stand-in, same key or not"
    );
}

#[test]
fn nothing_playing_still_draws_the_placeholder() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);

    cache.set_image(media("a"), None, None);
    assert!(!cache.prepare(
        Some(&picker),
        ArtworkMode::Blocks,
        Some(Rect::new(0, 0, 14, 7))
    ));
    assert!(
        cache.widget().is_none(),
        "with no kind there is no stand-in to show"
    );
}
