//! The `tenuto` command-line surface.

use std::num::NonZeroUsize;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "tenuto",
    version,
    about = "A keyboard-first terminal audio player"
)]
pub struct Cli {
    /// The subcommand to run. Absent means a bare `tenuto`, which opens
    /// the full-screen player on the saved queue with the same defaults
    /// `tenuto tui` uses when its flags are omitted.
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

/// Whether `tenuto tui` captures the mouse. Off leaves the terminal's own
/// selection and scrolling alone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum MouseMode {
    #[default]
    On,
    Off,
}

/// How `tenuto tui` draws cover art: `auto` asks the terminal which image
/// protocol it supports and falls back to colored half-blocks, `blocks`
/// always uses half-blocks without asking, and `off` shows only the
/// placeholder and never loads artwork.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum ArtworkMode {
    #[default]
    Auto,
    Blocks,
    Off,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Play a local audio file, an HTTP(S) URL, or a subscribed feed's
    /// episode by index.
    Play {
        /// Path to an MP3, FLAC, WAV or M4A file, an http(s):// URL, or —
        /// with an index after it — a subscribed feed's slug.
        source: String,
        /// The 1-based episode index within `source`'s cached episode list,
        /// as `tenuto episodes <slug>` displays it (§6.3).
        #[arg(value_parser = positive_index)]
        index: Option<NonZeroUsize>,
        /// Open the source, print what was found, and exit without using a
        /// device or a terminal. With an index, this applies after the
        /// episode is resolved (§6.3).
        #[arg(long)]
        probe_only: bool,
    },
    /// Subscribe to a podcast feed.
    Subscribe {
        /// The feed's http(s):// URL.
        url: String,
        /// Use this slug instead of one derived from the feed's title.
        #[arg(long = "as")]
        slug: Option<String>,
    },
    /// Remove a subscription and its cached episodes. Checkpoints are kept.
    Unsubscribe {
        /// The slug `tenuto feeds` displays.
        slug: String,
    },
    /// Open the terminal player on the saved queue.
    Tui {
        /// Whether the player captures the mouse.
        #[arg(long, value_enum, default_value_t = MouseMode::On)]
        mouse: MouseMode,
        /// How the player draws cover art.
        #[arg(long, value_enum, default_value_t = ArtworkMode::Auto)]
        artwork: ArtworkMode,
    },
    /// List every subscription.
    Feeds,
    /// Refresh one subscription, or every subscription when no slug is given.
    Refresh {
        /// The slug to refresh; omit to refresh all of them.
        slug: Option<String>,
    },
    /// List a subscription's cached episodes and their playback progress.
    Episodes {
        /// The slug `tenuto feeds` displays.
        slug: String,
        /// Display only the first N episodes. Indices never change (§6.3).
        #[arg(short = 'n', value_parser = positive_count)]
        limit: Option<NonZeroUsize>,
        /// Start from the end of the feed. Every episode keeps its index, and
        /// `-n` then counts from the end. Feeds are not always newest-first,
        /// so this is the feed's order reversed, not a sort by date.
        #[arg(long)]
        reverse: bool,
    },
}

/// Episode indices are 1-based (§1.2), so `0` is not a smaller index — it is
/// not an index at all. The message names both `play` spellings because a
/// rejected second positional is exactly where the two forms get confused.
fn positive_index(value: &str) -> Result<NonZeroUsize, String> {
    value
        .parse::<usize>()
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| {
            "expected a positive episode index: play <path-or-url> or play <slug> <index>".into()
        })
}

/// §6.3: `-n N` requires N ≥ 1. A zero limit would display nothing while
/// reporting success, which is never what was asked for.
fn positive_count(value: &str) -> Result<NonZeroUsize, String> {
    value
        .parse::<usize>()
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| "expected a positive count".into())
}
