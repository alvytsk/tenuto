//! Position provenance (§3): whether an absolute media time was
//! decoder-established or derived from a byte-offset estimate.
//!
//! **A second axis, orthogonal to `PositionQuality`** (`playback::timeline`), never
//! a fourth variant of it and never merged into it. `PositionQuality`
//! reports how precisely we know how much has been *heard*, reconstructed
//! from the output callback's spans — the ordinary state during playback.
//! This type reports whether the absolute media time itself is trustworthy.
//! A `Degraded` position (a timing base that jumped) whose media time the
//! decoder established is still `Established`; the two facts are unrelated
//! and both travel on `Progress` and `SeekCompleted` side by side.
//!
//! **Stronger than the word "estimated" suggests.** On a VBR MP3 with no
//! Xing/Info/VBRI tag, a byte-offset landing measured 235 s of error on a
//! 600 s file — asked for 360 s, it landed at 595 s while self-reporting a
//! plausible ~360 s. `Estimated` means "may name a substantially different
//! part of the recording," never "approximately right," and no accuracy
//! figure is promised.
//!
//! **Sticky (§3.1).** Decoding forward from an estimated landing keeps
//! reporting `Estimated`; no amount of elapsed playback converts it to
//! `Established`. Only something that independently re-establishes the
//! absolute position does: a confirmed seek landing, an established
//! restart, or a fresh load.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum PositionProvenance {
    /// The decoder established this media time.
    #[default]
    Established,
    /// Derived from a byte-offset estimate; the true media time may differ
    /// substantially from the value reported alongside this.
    Estimated,
}
