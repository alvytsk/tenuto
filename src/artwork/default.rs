//! The built-in cover that stands in when a track has no artwork of its own.
//!
//! Three images ship in the binary, one per kind of thing this player
//! holds: a record for music, a broadcast wave for radio, a microphone for
//! podcasts. They are decoded at most once each and then shared, because
//! the cover region is re-encoded on every resize and the decode should not
//! be paid again for it.
//!
//! Nothing here can fail loudly. The bytes are ours and a decode failure is
//! not reachable in a build that compiled, but the artwork module's rule is
//! that nothing in it takes the TUI down, so a failure yields `None` and
//! the caller falls back to the drawn placeholder.

use std::sync::{Arc, OnceLock};

use image::DynamicImage;

/// Which built-in cover stands in for a missing one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverKind {
    /// Local files and finite remote media.
    Music,
    /// A saved station, or any stream the engine reports as indefinite.
    Radio,
    /// A podcast episode.
    Podcast,
}

const MUSIC: &[u8] = include_bytes!("../../assets/cover-music.jpg");
const RADIO: &[u8] = include_bytes!("../../assets/cover-radio.jpg");
const PODCAST: &[u8] = include_bytes!("../../assets/cover-podcast.jpg");

/// The shared, decoded built-in cover for `kind`.
pub fn default_cover(kind: CoverKind) -> Option<Arc<DynamicImage>> {
    /// One slot per kind, so a decode is paid once per kind per run.
    static DECODED: [OnceLock<Option<Arc<DynamicImage>>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];

    let (slot, bytes) = match kind {
        CoverKind::Music => (&DECODED[0], MUSIC),
        CoverKind::Radio => (&DECODED[1], RADIO),
        CoverKind::Podcast => (&DECODED[2], PODCAST),
    };
    slot.get_or_init(|| {
        // The format is named rather than sniffed: these bytes are ours, so
        // there is nothing to detect and nothing to be misled by.
        image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
            .map(Arc::new)
            .ok()
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [CoverKind; 3] = [CoverKind::Music, CoverKind::Radio, CoverKind::Podcast];

    #[test]
    fn every_kind_decodes_to_a_square_cover() {
        for kind in KINDS {
            let cover = default_cover(kind).expect("a built-in cover decodes");
            assert_eq!(
                (cover.width(), cover.height()),
                (256, 256),
                "{kind:?} is not the expected size"
            );
        }
    }

    #[test]
    fn each_kind_is_a_different_picture() {
        let covers: Vec<_> = KINDS
            .into_iter()
            .map(|kind| default_cover(kind).expect("a built-in cover decodes"))
            .collect();
        for (left, right) in [(0, 1), (0, 2), (1, 2)] {
            assert_ne!(
                covers[left].as_bytes(),
                covers[right].as_bytes(),
                "{:?} and {:?} are the same picture",
                KINDS[left],
                KINDS[right]
            );
        }
    }

    #[test]
    fn the_same_kind_shares_one_decode() {
        let first = default_cover(CoverKind::Music).expect("a built-in cover decodes");
        let second = default_cover(CoverKind::Music).expect("a built-in cover decodes");
        assert!(
            Arc::ptr_eq(&first, &second),
            "each call decoded the image again"
        );
    }
}
