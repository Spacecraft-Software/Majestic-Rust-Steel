// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Resolves an `alacritty_terminal` cell [`Color`] to a concrete [`Rgb`] for rendering.
//!
//! Truecolor (`Spec`) passes through; named/indexed colors resolve against the standard
//! xterm palette. The terminal's default foreground/background and cursor map onto the
//! active [`Theme`] tokens so a shell blends into the Steelbore palette.

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use penumbra::{Rgb, Theme};

/// The 16 standard ANSI colors (xterm/VGA values).
const ANSI16: [Rgb; 16] = [
    Rgb::new(0, 0, 0),       // 0 black
    Rgb::new(128, 0, 0),     // 1 red
    Rgb::new(0, 128, 0),     // 2 green
    Rgb::new(128, 128, 0),   // 3 yellow
    Rgb::new(0, 0, 128),     // 4 blue
    Rgb::new(128, 0, 128),   // 5 magenta
    Rgb::new(0, 128, 128),   // 6 cyan
    Rgb::new(192, 192, 192), // 7 white
    Rgb::new(128, 128, 128), // 8 bright black
    Rgb::new(255, 0, 0),     // 9 bright red
    Rgb::new(0, 255, 0),     // 10 bright green
    Rgb::new(255, 255, 0),   // 11 bright yellow
    Rgb::new(0, 0, 255),     // 12 bright blue
    Rgb::new(255, 0, 255),   // 13 bright magenta
    Rgb::new(0, 255, 255),   // 14 bright cyan
    Rgb::new(255, 255, 255), // 15 bright white
];

/// Resolves a terminal cell color to an RGB value for the given theme.
pub(crate) fn resolve(color: Color, theme: &Theme) -> Rgb {
    match color {
        Color::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index),
        Color::Named(named) => match named {
            NamedColor::Foreground => theme.foreground,
            NamedColor::Background => theme.background,
            NamedColor::Cursor => theme.accent,
            other => indexed(named_index(other)),
        },
    }
}

/// Maps a named ANSI color to its palette index (non-ANSI names fall back to white).
fn named_index(named: NamedColor) -> u8 {
    match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => 7, // White and any non-ANSI name (Dim*, etc.) -> white
    }
}

/// Resolves an xterm 256-color index: 0–15 ANSI, 16–231 the 6×6×6 cube, 232–255 grayscale.
fn indexed(index: u8) -> Rgb {
    match index {
        0..=15 => ANSI16[usize::from(index)],
        16..=231 => {
            let c = index - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            Rgb::new(level(c / 36), level((c % 36) / 6), level(c % 6))
        }
        gray => {
            let value = 8 + 10 * (gray - 232);
            Rgb::new(value, value, value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{indexed, resolve, ANSI16};
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb as VteRgb};
    use penumbra::{Rgb, Theme};

    #[test]
    fn truecolor_passes_through() {
        let theme = Theme::steelbore();
        let color = Color::Spec(VteRgb {
            r: 10,
            g: 20,
            b: 30,
        });
        assert_eq!(resolve(color, &theme), Rgb::new(10, 20, 30));
    }

    #[test]
    fn named_default_uses_theme_tokens() {
        let theme = Theme::steelbore();
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), &theme),
            theme.foreground
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &theme),
            theme.background
        );
        assert_eq!(resolve(Color::Named(NamedColor::Red), &theme), ANSI16[1]);
    }

    #[test]
    fn indexed_palette_regions() {
        assert_eq!(indexed(1), ANSI16[1]); // ANSI
        assert_eq!(indexed(16), Rgb::new(0, 0, 0)); // cube origin
        assert_eq!(indexed(231), Rgb::new(255, 255, 255)); // cube max
        assert_eq!(indexed(232), Rgb::new(8, 8, 8)); // grayscale start
        assert_eq!(indexed(255), Rgb::new(238, 238, 238)); // grayscale end
    }
}
