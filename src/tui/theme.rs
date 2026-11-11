//! Built-in palettes: warm gruvbox greys for borders, cream text, green for
//! what plays and yellow for the selection — plus three dark alternatives
//! picked by [`ThemeId`]. A theme whose `bg` is `None` paints no background of
//! its own, so the terminal's background shows through; the dark themes paint
//! their own `bg` so their palettes stay legible on any terminal.

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    /// Dark text drawn on green highlights.
    pub ink: Color,
    pub panel: Color,
    pub line: Color,
    pub muted: Color,
    pub text: Color,
    pub cream: Color,
    pub green: Color,
    pub amber: Color,
    pub cyan: Color,
    /// Full-screen background the theme paints over the terminal's own, if any.
    pub bg: Option<Color>,
}

/// The gruvbox-dark palette: the original default, shared by
/// `Theme::default` and gruvbox's [`THEMES`] row so the two cannot drift.
const GRUVBOX: Theme = Theme {
    ink: Color::Rgb(0x28, 0x28, 0x28),
    panel: Color::Rgb(0x3c, 0x38, 0x36),
    line: Color::Rgb(0x50, 0x49, 0x45),
    muted: Color::Rgb(0x92, 0x83, 0x74),
    text: Color::Rgb(0xeb, 0xdb, 0xb2),
    cream: Color::Rgb(0xfb, 0xf1, 0xc7),
    green: Color::Rgb(0xb8, 0xbb, 0x26),
    amber: Color::Rgb(0xfa, 0xbd, 0x2f),
    cyan: Color::Rgb(0x8e, 0xc0, 0x7c),
    bg: None,
};

impl Default for Theme {
    fn default() -> Self {
        GRUVBOX
    }
}

/// Which built-in palette the interface uses, stored under its
/// [`ThemeId::as_stored`] spelling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeId {
    #[default]
    Gruvbox,
    Winamp,
    Nord,
    SolarizedDark,
}

/// Every built-in theme, in a fixed order: each [`ThemeId`] has exactly one
/// row, and [`ThemeId::theme`] returns that row.
pub const THEMES: [(ThemeId, Theme); 4] = [
    (ThemeId::Gruvbox, GRUVBOX),
    (
        ThemeId::Winamp,
        Theme {
            ink: Color::Rgb(0x00, 0x00, 0x00),
            panel: Color::Rgb(0x2a, 0x2a, 0x2a),
            line: Color::Rgb(0x4a, 0x4a, 0x4a),
            muted: Color::Rgb(0x7d, 0x7d, 0x7d),
            text: Color::Rgb(0x33, 0xff, 0x33),
            cream: Color::Rgb(0x99, 0xff, 0x99),
            green: Color::Rgb(0x33, 0xff, 0x33),
            amber: Color::Rgb(0xff, 0xaa, 0x33),
            cyan: Color::Rgb(0x33, 0x99, 0xcc),
            bg: Some(Color::Rgb(0x0a, 0x0a, 0x0a)),
        },
    ),
    (
        ThemeId::Nord,
        Theme {
            ink: Color::Rgb(0x2e, 0x34, 0x40),
            panel: Color::Rgb(0x3b, 0x42, 0x52),
            line: Color::Rgb(0x4c, 0x56, 0x6a),
            muted: Color::Rgb(0x61, 0x6e, 0x88),
            text: Color::Rgb(0xd8, 0xde, 0xe9),
            cream: Color::Rgb(0xec, 0xef, 0xf4),
            green: Color::Rgb(0xa3, 0xbe, 0x8c),
            amber: Color::Rgb(0xeb, 0xcb, 0x8b),
            cyan: Color::Rgb(0x88, 0xc0, 0xd0),
            bg: Some(Color::Rgb(0x2e, 0x34, 0x40)),
        },
    ),
    (
        ThemeId::SolarizedDark,
        Theme {
            ink: Color::Rgb(0x00, 0x2b, 0x36),
            panel: Color::Rgb(0x07, 0x36, 0x42),
            line: Color::Rgb(0x58, 0x6e, 0x75),
            muted: Color::Rgb(0x65, 0x7b, 0x83),
            text: Color::Rgb(0x83, 0x94, 0x96),
            cream: Color::Rgb(0xee, 0xe8, 0xd5),
            green: Color::Rgb(0x85, 0x99, 0x00),
            amber: Color::Rgb(0xb5, 0x89, 0x00),
            cyan: Color::Rgb(0x2a, 0xa1, 0x98),
            bg: Some(Color::Rgb(0x00, 0x2b, 0x36)),
        },
    ),
];

impl ThemeId {
    /// Parses the exact spelling written to storage; `None` for anything else
    /// (case-sensitive: `"Nord"` is not a spelling).
    pub fn from_stored(raw: &str) -> Option<Self> {
        match raw {
            "gruvbox" => Some(Self::Gruvbox),
            "winamp" => Some(Self::Winamp),
            "nord" => Some(Self::Nord),
            "solarized-dark" => Some(Self::SolarizedDark),
            _ => None,
        }
    }

    /// The exact spelling written to storage. Total over the four variants:
    /// an explicitly chosen theme always has a spelling.
    pub fn as_stored(self) -> Option<&'static str> {
        match self {
            Self::Gruvbox => Some("gruvbox"),
            Self::Winamp => Some("winamp"),
            Self::Nord => Some("nord"),
            Self::SolarizedDark => Some("solarized-dark"),
        }
    }

    /// The palette this id names: its single row in [`THEMES`]. Exhaustive
    /// over the enum, so a variant without a table row fails to compile.
    pub fn theme(self) -> Theme {
        match self {
            Self::Gruvbox => THEMES[0].1,
            Self::Winamp => THEMES[1].1,
            Self::Nord => THEMES[2].1,
            Self::SolarizedDark => THEMES[3].1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{THEMES, ThemeId};

    #[test]
    fn the_table_covers_every_theme_exactly_once() {
        assert_eq!(THEMES.len(), 4);
        assert_eq!((ThemeId::Gruvbox, ThemeId::Gruvbox.theme()), THEMES[0]);
        assert_eq!((ThemeId::Winamp, ThemeId::Winamp.theme()), THEMES[1]);
        assert_eq!((ThemeId::Nord, ThemeId::Nord.theme()), THEMES[2]);
        assert_eq!(
            (ThemeId::SolarizedDark, ThemeId::SolarizedDark.theme()),
            THEMES[3]
        );
        // Uniqueness: four distinct ThemeIds.
        for (i, (id, _)) in THEMES.iter().enumerate() {
            for (j, (other, _)) in THEMES.iter().enumerate().skip(i + 1) {
                assert_ne!(id, other, "rows {i} and {j} share an id");
            }
        }
    }

    #[test]
    fn the_three_dark_themes_paint_a_background_and_gruvbox_does_not() {
        assert_eq!(ThemeId::Gruvbox.theme().bg, None);
        for id in [ThemeId::Winamp, ThemeId::Nord, ThemeId::SolarizedDark] {
            assert!(id.theme().bg.is_some(), "{id:?} must paint its own bg");
        }
    }

    #[test]
    fn no_theme_ships_a_placeholder() {
        for (_, theme) in THEMES {
            for color in [
                theme.ink,
                theme.panel,
                theme.line,
                theme.muted,
                theme.text,
                theme.cream,
                theme.green,
                theme.amber,
                theme.cyan,
            ] {
                assert_ne!(color, ratatui::style::Color::Reset);
            }
        }
    }

    #[test]
    fn gruvbox_stays_bytes_identical_to_todays_palette() {
        let theme = ThemeId::Gruvbox.theme();
        assert_eq!(theme.ink, ratatui::style::Color::Rgb(0x28, 0x28, 0x28));
        assert_eq!(theme.panel, ratatui::style::Color::Rgb(0x3c, 0x38, 0x36));
        assert_eq!(theme.line, ratatui::style::Color::Rgb(0x50, 0x49, 0x45));
        assert_eq!(theme.muted, ratatui::style::Color::Rgb(0x92, 0x83, 0x74));
        assert_eq!(theme.text, ratatui::style::Color::Rgb(0xeb, 0xdb, 0xb2));
        assert_eq!(theme.cream, ratatui::style::Color::Rgb(0xfb, 0xf1, 0xc7));
        assert_eq!(theme.green, ratatui::style::Color::Rgb(0xb8, 0xbb, 0x26));
        assert_eq!(theme.amber, ratatui::style::Color::Rgb(0xfa, 0xbd, 0x2f));
        assert_eq!(theme.cyan, ratatui::style::Color::Rgb(0x8e, 0xc0, 0x7c));
    }

    #[test]
    fn storage_round_trips_all_four_spellings() {
        for (raw, id) in [
            ("gruvbox", ThemeId::Gruvbox),
            ("winamp", ThemeId::Winamp),
            ("nord", ThemeId::Nord),
            ("solarized-dark", ThemeId::SolarizedDark),
        ] {
            assert_eq!(ThemeId::from_stored(raw), Some(id));
            assert_eq!(id.as_stored(), Some(raw));
        }
        assert_eq!(ThemeId::from_stored("dracula"), None);
        assert_eq!(ThemeId::from_stored("Nord"), None);
    }
}
