// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Colors, cell [`Style`], and the named [`Theme`] (Standard §9 / §9.1).
//!
//! UI logic references colors through [`Theme`] tokens, never bare hex literals — the only
//! place the palette's hex values appear is [`Theme::steelbore`], which *is* the color
//! contract (mirroring `themes/steelbore.toml`). Swapping the theme reskins the editor
//! without touching any drawing code.

/// A 24-bit truecolor value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Creates a color from its channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Text attributes for a cell.
#[expect(
    clippy::struct_excessive_bools,
    reason = "terminal cell attributes are independent flags; a bitset would be less ergonomic"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    /// Bold / increased intensity.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Reverse video (swap fg/bg).
    pub reverse: bool,
}

impl Attrs {
    /// No attributes set.
    pub const NONE: Self = Self {
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    };
}

/// A foreground color, background color, and attribute set for a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Foreground (text) color.
    pub fg: Rgb,
    /// Background color.
    pub bg: Rgb,
    /// Text attributes.
    pub attrs: Attrs,
}

impl Style {
    /// Creates a plain style (no attributes) with the given colors.
    #[must_use]
    pub const fn new(fg: Rgb, bg: Rgb) -> Self {
        Self {
            fg,
            bg,
            attrs: Attrs::NONE,
        }
    }

    /// Returns this style with bold enabled.
    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.attrs.bold = true;
        self
    }
}

/// The six-token color contract (Standard §9.1). Default is the canonical `Steelbore` theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Void Navy — background / canvas (the mandatory Spacecraft background).
    pub background: Rgb,
    /// Molten Amber — primary text / active readout.
    pub foreground: Rgb,
    /// Steel Blue — primary accent / structural lines.
    pub accent: Rgb,
    /// Radium Green — success / safe status.
    pub success: Rgb,
    /// Red Oxide — warning / error status.
    pub error: Rgb,
    /// Liquid Coolant — info / links.
    pub info: Rgb,
}

impl Theme {
    /// The canonical `Steelbore` theme (Standard §9). Hex values mirror `themes/steelbore.toml`.
    #[must_use]
    pub const fn steelbore() -> Self {
        Self {
            background: Rgb::new(0x00, 0x00, 0x27), // Void Navy (mandatory)
            foreground: Rgb::new(0xD9, 0x8E, 0x32), // Molten Amber
            accent: Rgb::new(0x4B, 0x7E, 0xB0),     // Steel Blue
            success: Rgb::new(0x50, 0xFA, 0x7B),    // Radium Green
            error: Rgb::new(0xFF, 0x5C, 0x5C),      // Red Oxide
            info: Rgb::new(0x8B, 0xE9, 0xFD),       // Liquid Coolant
        }
    }

    /// The base text style: [`foreground`](Theme::foreground) on [`background`](Theme::background).
    #[must_use]
    pub const fn base_style(self) -> Style {
        Style::new(self.foreground, self.background)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::steelbore()
    }
}

#[cfg(test)]
mod tests {
    use super::{Rgb, Theme};

    #[test]
    fn steelbore_tokens_match_the_standard() {
        let theme = Theme::steelbore();
        assert_eq!(theme.background, Rgb::new(0, 0, 39)); // #000027 Void Navy
        assert_eq!(theme.foreground, Rgb::new(217, 142, 50)); // #D98E32
        assert_eq!(theme.info, Rgb::new(139, 233, 253)); // #8BE9FD
        assert_eq!(Theme::default(), theme);
    }
}
