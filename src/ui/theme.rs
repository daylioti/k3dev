use ratatui::style::Color;
use serde::Deserialize;

/// Available UI themes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Classic Fallout terminal - green phosphor on dark background
    #[default]
    Fallout,
    /// Cyberpunk neon - purple and cyan on dark
    Cyberpunk,
    /// Nord - calm blue-gray palette
    Nord,
}

impl Theme {
    /// Get the color palette for this theme
    pub fn palette(&self) -> ColorPalette {
        match self {
            Theme::Fallout => ColorPalette::fallout(),
            Theme::Cyberpunk => ColorPalette::cyberpunk(),
            Theme::Nord => ColorPalette::nord(),
        }
    }

    /// Parse theme from string (for config)
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "fallout" => Some(Theme::Fallout),
            "cyberpunk" => Some(Theme::Cyberpunk),
            "nord" => Some(Theme::Nord),
            _ => None,
        }
    }
}

/// Color palette for theming
#[derive(Clone)]
pub struct ColorPalette {
    /// Primary accent color (selections, focused borders)
    pub primary: Color,
    /// Secondary accent color (headers, titles)
    #[allow(dead_code)]
    pub secondary: Color,
    /// Success indicator color
    pub success: Color,
    /// Warning indicator color
    pub warning: Color,
    /// Error indicator color
    pub error: Color,
    /// Muted/disabled text color
    pub muted: Color,
    /// Background color
    #[allow(dead_code)]
    pub background: Color,
    /// Border color (unfocused)
    pub border: Color,
    /// Normal text color
    pub text: Color,
    /// Highlight color (bright accents)
    pub highlight: Color,
    /// Selection background color
    pub selection_bg: Color,
}

impl ColorPalette {
    /// Fallout terminal theme - green phosphor CRT aesthetic
    pub fn fallout() -> Self {
        Self {
            primary: Color::Rgb(32, 194, 14),     // Bright phosphor green
            secondary: Color::Rgb(24, 160, 24),   // Medium phosphor green
            success: Color::Rgb(51, 255, 51),     // Bright success green
            warning: Color::Rgb(196, 160, 0),     // Amber phosphor
            error: Color::Rgb(204, 51, 51),       // Red phosphor
            muted: Color::Rgb(10, 95, 10),        // Dim phosphor green
            background: Color::Rgb(10, 10, 10),   // CRT black
            border: Color::Rgb(15, 111, 15),      // Terminal border green
            text: Color::Rgb(32, 194, 14),        // Standard phosphor green
            highlight: Color::Rgb(64, 255, 64),   // Bright highlight
            selection_bg: Color::Rgb(15, 50, 15), // Dark green selection
        }
    }

    /// Cyberpunk theme - neon purple and cyan
    pub fn cyberpunk() -> Self {
        Self {
            primary: Color::Rgb(189, 147, 249),   // Neon purple
            secondary: Color::Rgb(139, 233, 253), // Cyan
            success: Color::Rgb(80, 250, 123),    // Neon green
            warning: Color::Rgb(255, 184, 108),   // Orange
            error: Color::Rgb(255, 85, 85),       // Neon red
            muted: Color::Rgb(98, 114, 164),      // Muted blue-gray
            background: Color::Rgb(13, 17, 23),   // Deep dark blue
            border: Color::Rgb(68, 71, 90),       // Dark purple-gray
            text: Color::Rgb(248, 248, 242),      // Off-white
            highlight: Color::Rgb(255, 121, 198), // Hot pink
            selection_bg: Color::Rgb(68, 71, 90), // Selection purple
        }
    }

    /// Nord theme - calm arctic blue-gray palette
    pub fn nord() -> Self {
        Self {
            primary: Color::Rgb(136, 192, 208),   // Nord frost blue
            secondary: Color::Rgb(129, 161, 193), // Nord blue
            success: Color::Rgb(163, 190, 140),   // Nord green
            warning: Color::Rgb(235, 203, 139),   // Nord yellow
            error: Color::Rgb(191, 97, 106),      // Nord red
            muted: Color::Rgb(76, 86, 106),       // Nord gray
            background: Color::Rgb(46, 52, 64),   // Nord polar night
            border: Color::Rgb(67, 76, 94),       // Nord dark gray
            text: Color::Rgb(236, 239, 244),      // Nord snow storm
            highlight: Color::Rgb(94, 129, 172),  // Nord bright blue
            selection_bg: Color::Rgb(67, 76, 94), // Nord selection
        }
    }
}

impl Default for ColorPalette {
    fn default() -> Self {
        Self::fallout()
    }
}
