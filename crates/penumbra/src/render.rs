// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame diffing and minimal VT emission — the rendering hot path.
//!
//! [`render`] compares the previously displayed [`Buffer`] with the next one and writes only
//! the changed cells as VT escapes: a cursor move (`CUP`) is emitted only when the next
//! changed cell is not adjacent to the last, and a color/attribute change (`SGR`) only when
//! the style actually differs. This keeps output small — efficient over SSH. [`Screen`]
//! wraps the front/back buffers so callers just draw and [`Screen::present`].

use std::io::{self, Write};

use crate::buffer::{char_width, Buffer};
use crate::theme::Style;

/// A double-buffered surface: draw into the back buffer, then [`present`](Screen::present).
#[derive(Debug)]
pub struct Screen {
    front: Buffer,
    back: Buffer,
}

impl Screen {
    /// Creates a screen of `width × height` blank cells in `fill`.
    #[must_use]
    pub fn new(width: u16, height: u16, fill: Style) -> Self {
        Self {
            front: Buffer::new(width, height, fill),
            back: Buffer::new(width, height, fill),
        }
    }

    /// The back buffer to draw the next frame into.
    pub fn back_mut(&mut self) -> &mut Buffer {
        &mut self.back
    }

    /// The currently displayed front buffer.
    #[must_use]
    pub fn front(&self) -> &Buffer {
        &self.front
    }

    /// Resizes both buffers, clearing them to blank cells in `fill`.
    pub fn resize(&mut self, width: u16, height: u16, fill: Style) {
        self.front.resize(width, height, fill);
        self.back.resize(width, height, fill);
    }

    /// Emits the diff between the displayed frame and the back buffer, then adopts it.
    ///
    /// # Errors
    /// Returns any I/O error from writing to `out`.
    pub fn present(&mut self, out: &mut impl Write) -> io::Result<()> {
        render(&self.front, &self.back, out)?;
        self.front.clone_from(&self.back);
        Ok(())
    }
}

/// Writes the minimal VT escapes that turn `prev` into `next` on `out`.
///
/// If the buffers differ in size, the screen is cleared and `next` is drawn in full;
/// otherwise only changed cells are emitted.
///
/// # Errors
/// Returns any I/O error from writing to `out`.
pub fn render(prev: &Buffer, next: &Buffer, out: &mut impl Write) -> io::Result<()> {
    let full_redraw = prev.width() != next.width() || prev.height() != next.height();
    if full_redraw {
        out.write_all(b"\x1b[2J")?;
    }

    let mut cursor: Option<(u16, u16)> = None;
    let mut current: Option<Style> = None;

    for y in 0..next.height() {
        for x in 0..next.width() {
            let Some(cell) = next.cell(x, y) else {
                continue;
            };
            // The trailing half of a double-width glyph is covered by the glyph itself — never
            // emit it (doing so would desync the terminal cursor by a column).
            if cell.is_continuation() {
                continue;
            }
            let changed = full_redraw || prev.cell(x, y) != Some(cell);
            if !changed {
                continue;
            }

            if cursor != Some((x, y)) {
                write_cursor(out, x, y)?;
            }
            if current != Some(cell.style) {
                write_style(out, cell.style)?;
                current = Some(cell.style);
            }

            let mut utf8 = [0u8; 4];
            out.write_all(cell.symbol.encode_utf8(&mut utf8).as_bytes())?;

            // Advance by the glyph's display width (a wide glyph moves the cursor two columns);
            // force a reposition at row end to avoid depending on autowrap behavior.
            let next_x = x.saturating_add(char_width(cell.symbol));
            cursor = (next_x < next.width()).then_some((next_x, y));
        }
    }
    out.flush()
}

/// Emits a Cursor Position (`CUP`) escape for the one-based `(row, col)` of `(x, y)`.
fn write_cursor(out: &mut impl Write, x: u16, y: u16) -> io::Result<()> {
    write!(out, "\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1)
}

/// Emits a canonical `SGR` escape: reset, then attributes, then truecolor fg and bg.
fn write_style(out: &mut impl Write, style: Style) -> io::Result<()> {
    out.write_all(b"\x1b[0")?;
    if style.attrs.bold {
        out.write_all(b";1")?;
    }
    if style.attrs.italic {
        out.write_all(b";3")?;
    }
    if style.attrs.underline {
        out.write_all(b";4")?;
    }
    if style.attrs.reverse {
        out.write_all(b";7")?;
    }
    write!(
        out,
        ";38;2;{};{};{};48;2;{};{};{}m",
        style.fg.r, style.fg.g, style.fg.b, style.bg.r, style.bg.g, style.bg.b
    )
}

#[cfg(test)]
mod tests {
    use super::{render, Screen};
    use crate::buffer::{Buffer, Cell};
    use crate::theme::{Attrs, Rgb, Style, Theme};

    fn base() -> Style {
        Theme::steelbore().base_style()
    }

    /// Replays the renderer's own VT grammar (CUP / SGR / clear / ASCII) onto a copy of
    /// `prev`, reconstructing what a terminal would show. ASCII glyphs only (test inputs).
    fn reconstruct(start: &Buffer, bytes: &[u8]) -> Buffer {
        let mut buf = start.clone();
        let mut cursor = (0u16, 0u16);
        let mut style = base();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                let params = std::str::from_utf8(&bytes[i + 2..j]).unwrap();
                match bytes[j] {
                    b'H' => {
                        let mut it = params.split(';');
                        let row: u16 = it.next().unwrap_or("1").parse().unwrap_or(1);
                        let col: u16 = it.next().unwrap_or("1").parse().unwrap_or(1);
                        cursor = (col - 1, row - 1);
                    }
                    b'm' => apply_sgr(params, &mut style),
                    b'J' => buf.clear(base()),
                    _ => {}
                }
                i = j + 1;
            } else {
                buf.set(cursor.0, cursor.1, Cell::new(bytes[i] as char, style));
                cursor.0 += 1;
                i += 1;
            }
        }
        buf
    }

    fn apply_sgr(params: &str, style: &mut Style) {
        let mut it = params.split(';').peekable();
        while let Some(p) = it.next() {
            match p {
                "0" | "" => style.attrs = Attrs::NONE,
                "1" => style.attrs.bold = true,
                "3" => style.attrs.italic = true,
                "4" => style.attrs.underline = true,
                "7" => style.attrs.reverse = true,
                "38" | "48" => {
                    let is_fg = p == "38";
                    let _mode = it.next(); // "2" (truecolor)
                    let r = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let g = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let b = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    let color = Rgb::new(r, g, b);
                    if is_fg {
                        style.fg = color;
                    } else {
                        style.bg = color;
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn no_change_emits_nothing() {
        let buffer = Buffer::new(8, 3, base());
        let mut out = Vec::new();
        render(&buffer, &buffer, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn single_change_reconstructs() {
        let prev = Buffer::new(8, 3, base());
        let mut next = prev.clone();
        next.set_char(4, 1, 'Z', base());
        let mut out = Vec::new();
        render(&prev, &next, &mut out).unwrap();
        assert!(!out.is_empty());
        assert_eq!(reconstruct(&prev, &out), next);
    }

    #[test]
    fn full_redraw_on_resize_reconstructs() {
        let prev = Buffer::new(4, 2, base());
        let mut next = Buffer::new(6, 2, base()); // different size -> full redraw
        next.set_str(0, 0, "hello", base().bold());
        next.set_str(
            0,
            1,
            "world",
            Style::new(Theme::steelbore().error, Theme::steelbore().background),
        );
        let mut out = Vec::new();
        render(&prev, &next, &mut out).unwrap();
        // On resize the terminal is already the new size, then cleared and fully redrawn,
        // so reconstruction starts from a blank buffer of the new dimensions.
        assert_eq!(reconstruct(&Buffer::new(6, 2, base()), &out), next);
    }

    #[test]
    fn many_scattered_changes_reconstruct() {
        let prev = Buffer::new(20, 6, base());
        let mut next = prev.clone();
        next.set_str(1, 0, "fn main() {", base().bold());
        next.set_str(4, 1, "let x = 1;", base());
        next.set_str(
            0,
            5,
            "status",
            Style::new(Theme::steelbore().info, Theme::steelbore().accent),
        );
        let mut out = Vec::new();
        render(&prev, &next, &mut out).unwrap();
        assert_eq!(reconstruct(&prev, &out), next);
    }

    #[test]
    fn wide_glyph_emits_once_without_continuation() {
        let prev = Buffer::new(8, 1, base());
        let mut next = prev.clone();
        next.set_str(0, 0, "世", base());
        let mut out = Vec::new();
        render(&prev, &next, &mut out).unwrap();
        // The wide glyph is emitted, and its continuation cell (NUL) never is.
        assert!(
            out.windows(3).any(|window| window == "世".as_bytes()),
            "expected the wide glyph in the output"
        );
        assert!(
            !out.contains(&0u8),
            "the continuation cell must never be emitted"
        );
    }

    #[test]
    fn screen_present_then_idle_is_silent() {
        let mut screen = Screen::new(10, 2, base());
        screen.back_mut().set_str(0, 0, "hi", base());
        let mut out = Vec::new();
        screen.present(&mut out).unwrap();
        assert!(!out.is_empty());
        // Front now matches back; redrawing the same frame emits nothing.
        screen.back_mut().set_str(0, 0, "hi", base());
        let mut out2 = Vec::new();
        screen.present(&mut out2).unwrap();
        assert!(out2.is_empty());
    }
}
