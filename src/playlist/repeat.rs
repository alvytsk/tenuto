//! Winamp's repeat toggle (M13 §1.1): what happens when a track ends.
//!
//! Global, not per-playlist — one listening context exists per session, and
//! this is playback-order policy the *Session* applies where it already
//! computes the successor on `EndOfTrack`. Pure data, like the rest of
//! `playlist`; persistence maps it to a string in `persistence/model.rs`
//! and nowhere else.

/// What an ended track asks for next.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RepeatMode {
    /// Stop at the end; Space replays the last entry (the pre-M13 rule).
    #[default]
    Off,
    /// Play the ended entry again, from zero (a completed entry replays from
    /// zero by D1, so this falls out of the ordinary reload path).
    One,
    /// Wrap to the first entry of the playing playlist's playback order.
    /// The current shuffled order is kept and wrapped; it is not reshuffled
    /// at the seam.
    All,
}

impl RepeatMode {
    /// The `R` key's cycle: off → one → all → off.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::One,
            Self::One => Self::All,
            Self::All => Self::Off,
        }
    }

    /// The tab-strip badge (M13 §1.1): `None` when off, as shuffle's absence
    /// is shown by no suffix.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::One => Some("R:1"),
            Self::All => Some("R:*"),
        }
    }

    /// The persistence spelling (`RawState`'s additive field). Lenient by
    /// construction: an unknown string is refused here and the caller
    /// defaults to [`RepeatMode::Off`], the same policy `Volume::new`
    /// applies to a hand-edited gain.
    pub fn from_stored(raw: &str) -> Option<Self> {
        match raw {
            "one" => Some(Self::One),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_stored(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::One => Some("one"),
            Self::All => Some("all"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RepeatMode;

    #[test]
    fn the_cycle_returns_to_off() {
        assert_eq!(RepeatMode::Off.next(), RepeatMode::One);
        assert_eq!(RepeatMode::One.next(), RepeatMode::All);
        assert_eq!(RepeatMode::All.next(), RepeatMode::Off);
    }

    #[test]
    fn storage_round_trips_and_defaults_unknown_values() {
        // Off serializes as absent, so its roundtrip is the default.
        assert_eq!(RepeatMode::Off.as_stored(), None);
        for mode in [RepeatMode::One, RepeatMode::All] {
            let stored = mode.as_stored().expect("a spelling for a non-off mode");
            assert_eq!(RepeatMode::from_stored(stored), Some(mode));
        }
        assert_eq!(RepeatMode::from_stored("off"), None);
        assert_eq!(RepeatMode::from_stored("One"), None);
        assert_eq!(RepeatMode::from_stored(""), None);
    }

    #[test]
    fn the_badge_tracks_the_mode() {
        assert_eq!(RepeatMode::Off.badge(), None);
        assert_eq!(RepeatMode::One.badge(), Some("R:1"));
        assert_eq!(RepeatMode::All.badge(), Some("R:*"));
    }
}
