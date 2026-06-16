// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Terminal`] widget — an embedded `alacritty_terminal` grid rendered into Penumbra.
//!
//! [`Terminal`] owns an `alacritty_terminal` [`Term`] and a VT parser. Bytes from a child
//! program are pushed in with [`Terminal::feed`]; [`Terminal::render`] reads the resulting
//! cell grid (characters, colors, attributes) into a Penumbra [`Buffer`]. Spawning a real
//! shell over a PTY and pumping its output into [`Terminal::feed`] on a background thread is
//! the next majestic-term step; this layer is the headless-testable emulation core.

use std::fmt;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use penumbra::{Buffer, Rect, Style, Theme};

use crate::color::resolve;

/// Default scrollback retained by the terminal (PRD §6.6).
const SCROLLBACK_LINES: usize = 10_000;

/// A grid size for `alacritty_terminal` (no separate scrollback in the visible dimensions).
struct GridSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// An embedded terminal emulator: feed it child-program bytes, render its grid.
pub struct Terminal {
    term: Term<VoidListener>,
    parser: Processor,
    columns: usize,
    screen_lines: usize,
}

impl Terminal {
    /// Creates a terminal of `columns × screen_lines` (each clamped to at least one).
    #[must_use]
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        let columns = columns.max(1);
        let screen_lines = screen_lines.max(1);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &GridSize {
                columns,
                screen_lines,
            },
            VoidListener,
        );
        Self {
            term,
            parser: Processor::new(),
            columns,
            screen_lines,
        }
    }

    /// Feeds `bytes` (child-program output) through the VT parser into the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resizes the terminal grid (clamped to at least one column and line).
    pub fn resize(&mut self, columns: usize, screen_lines: usize) {
        self.columns = columns.max(1);
        self.screen_lines = screen_lines.max(1);
        self.term.resize(GridSize {
            columns: self.columns,
            screen_lines: self.screen_lines,
        });
    }

    /// The number of columns.
    #[must_use]
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// The number of visible lines.
    #[must_use]
    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    /// The underlying `alacritty_terminal` term (advanced escape hatch).
    #[must_use]
    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
    }

    /// Renders the visible grid into `surface` using `theme` for default colors.
    pub fn render(&self, surface: &mut Buffer, theme: &Theme) {
        self.render_in(surface, surface.area(), theme);
    }

    /// Renders the visible grid into `area` of `surface`, offsetting and clipping to it.
    ///
    /// Cells outside `area` (the grid is wider/taller than the panel) are clipped; the host is
    /// expected to keep the terminal sized to the panel via [`Terminal::resize`].
    pub fn render_in(&self, surface: &mut Buffer, area: Rect, theme: &Theme) {
        for indexed in self.term.grid().display_iter() {
            let cell = indexed.cell;
            let (Ok(row), Ok(col)) = (
                u16::try_from(indexed.point.line.0),
                u16::try_from(indexed.point.column.0),
            ) else {
                continue;
            };
            if row >= area.height || col >= area.width {
                continue;
            }

            let mut style = Style::new(resolve(cell.fg, theme), resolve(cell.bg, theme));
            style.attrs.bold = cell.flags.contains(Flags::BOLD);
            style.attrs.italic = cell.flags.contains(Flags::ITALIC);
            style.attrs.underline = cell.flags.contains(Flags::UNDERLINE);
            style.attrs.reverse = cell.flags.contains(Flags::INVERSE);

            let symbol = if cell.c == '\0' { ' ' } else { cell.c };
            surface.set_char(area.x + col, area.y + row, symbol, style);
        }
    }
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Terminal")
            .field("columns", &self.columns)
            .field("screen_lines", &self.screen_lines)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::Terminal;
    use crate::color::resolve;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    use penumbra::{Buffer, Theme};

    fn rendered(terminal: &Terminal, theme: &Theme) -> Buffer {
        let mut surface = Buffer::new(
            u16::try_from(terminal.columns()).unwrap(),
            u16::try_from(terminal.screen_lines()).unwrap(),
            theme.base_style(),
        );
        terminal.render(&mut surface, theme);
        surface
    }

    fn row_text(surface: &Buffer, row: u16) -> String {
        (0..surface.width())
            .filter_map(|col| surface.cell(col, row).map(|cell| cell.symbol))
            .collect::<String>()
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let theme = Theme::steelbore();
        let mut terminal = Terminal::new(10, 3);
        terminal.feed(b"hello");
        assert_eq!(row_text(&rendered(&terminal, &theme), 0), "hello     ");
    }

    #[test]
    fn crlf_starts_a_new_line() {
        let theme = Theme::steelbore();
        let mut terminal = Terminal::new(6, 3);
        terminal.feed(b"ab\r\ncd");
        let surface = rendered(&terminal, &theme);
        assert_eq!(row_text(&surface, 0), "ab    ");
        assert_eq!(row_text(&surface, 1), "cd    ");
    }

    #[test]
    fn sgr_color_is_applied_to_cells() {
        let theme = Theme::steelbore();
        let mut terminal = Terminal::new(8, 2);
        terminal.feed(b"\x1b[31mR\x1b[0mn"); // red 'R', then reset, then 'n'
        let surface = rendered(&terminal, &theme);
        let red = resolve(Color::Named(NamedColor::Red), &theme);
        assert_eq!(surface.cell(0, 0).unwrap().symbol, 'R');
        assert_eq!(surface.cell(0, 0).unwrap().style.fg, red);
        // The reset cell falls back to the default (themed) foreground.
        assert_eq!(surface.cell(1, 0).unwrap().symbol, 'n');
        assert_eq!(surface.cell(1, 0).unwrap().style.fg, theme.foreground);
    }

    #[test]
    fn bold_attribute_is_applied() {
        let theme = Theme::steelbore();
        let mut terminal = Terminal::new(4, 1);
        terminal.feed(b"\x1b[1mB");
        let surface = rendered(&terminal, &theme);
        assert!(surface.cell(0, 0).unwrap().style.attrs.bold);
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut terminal = Terminal::new(10, 4);
        terminal.resize(20, 6);
        assert_eq!(terminal.columns(), 20);
        assert_eq!(terminal.screen_lines(), 6);
        assert_eq!(Terminal::new(0, 0).columns(), 1); // clamped
    }
}
