// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP hover popup (PRD #1 §6.9).
//!
//! A bordered, scrollable box of documentation/type information anchored at the cursor — the
//! keyboard counterpart to mouse hover. The host requests hover from the language server
//! (off-thread), builds a [`Hover`] from the reply text, and draws it over the editor with
//! [`Hover::render`]; ↑/↓ scroll long content and any other key dismisses it. Editor-facing only:
//! the `lsp-types` payload is reduced to plain text before it reaches here, so the core carries no
//! LSP dependency (mirroring [`Completion`](crate::Completion)).

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// A hover popup: the documentation lines plus the index of the first visible line.
#[derive(Clone, Debug)]
pub struct Hover {
    lines: Vec<String>,
    /// The first line drawn — advanced by [`Hover::scroll_down`] so long content is reachable.
    scroll: usize,
}

/// Largest popup the box is allowed to grow to. Rows cap the height so a long doc string scrolls
/// rather than covering the editor; the width band keeps the box readable without dominating a
/// narrow terminal. A bordered box adds one row/column on each side, accounted for in `render`.
const MAX_ROWS: u16 = 12;
const MAX_WIDTH: u16 = 72;
const MIN_WIDTH: u16 = 14;

impl Hover {
    /// Builds a hover popup from `text` (the server's content reduced to plain text), one entry per
    /// line, with leading and trailing blank lines trimmed. The first line is shown at the top.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        while lines.first().is_some_and(|line| line.trim().is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        Self { lines, scroll: 0 }
    }

    /// Whether there is nothing to show (the host should not open an empty popup).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Scrolls up one line (saturating at the top).
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scrolls down one line (saturating at the last line, so content never scrolls out of reach).
    pub fn scroll_down(&mut self) {
        if self.scroll + 1 < self.lines.len() {
            self.scroll += 1;
        }
    }

    /// Draws the popup anchored at `cursor` (an absolute screen position), clamped inside `area`.
    /// Prefers to open just below the cursor, flipping above it when there is no room. Lines wider
    /// than the box are clipped so nothing draws over the border.
    pub fn render(&self, surface: &mut Surface, area: Rect, cursor: (u16, u16), theme: &Theme) {
        if self.lines.is_empty() || area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let rows = MAX_ROWS.min(u16::try_from(self.lines.len()).unwrap_or(MAX_ROWS));
        let height = rows + 2; // a bordered box adds a row top and bottom
        let content_width = self
            .lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        let width = u16::try_from(content_width + 2)
            .unwrap_or(MAX_WIDTH)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .min(area.width);

        // Place below the cursor, flipping above it when the box would fall off the bottom.
        let (cx, cy) = cursor;
        let x = cx.min(area.right().saturating_sub(width));
        let below = cy + 1;
        let y = if below + height <= area.bottom() {
            below
        } else {
            cy.saturating_sub(height).max(area.y)
        };
        let box_area = Rect::new(x, y, width, height);

        draw_box(surface, box_area, theme, "hover");
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        // Clamp the scroll so a stale offset (e.g. after a shorter popup) never indexes past the end.
        let start = self.scroll.min(self.lines.len().saturating_sub(1));
        let max_chars = usize::from(inner.width);
        for row in 0..inner.height {
            let Some(line) = self.lines.get(start + usize::from(row)) else {
                break;
            };
            let y = inner.y + row;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', fg);
            }
            // Clip to the inner width so an over-long line cannot overwrite the right border.
            let shown: String = line.chars().take(max_chars).collect();
            surface.set_str(inner.x, y, &shown, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Hover;

    #[test]
    fn trims_surrounding_blank_lines_but_keeps_interior() {
        let hover = Hover::new("\n\nfn foo()\n\nDocs here\n\n");
        assert_eq!(hover.lines, vec!["fn foo()", "", "Docs here"]);
        assert!(!hover.is_empty());
    }

    #[test]
    fn blank_content_is_empty() {
        assert!(Hover::new("").is_empty());
        assert!(Hover::new("   \n\n  ").is_empty());
    }

    #[test]
    fn scrolling_saturates_at_both_ends() {
        let mut hover = Hover::new("a\nb\nc");
        hover.scroll_up(); // already at top: no-op
        assert_eq!(hover.scroll, 0);
        hover.scroll_down();
        hover.scroll_down();
        hover.scroll_down(); // last line is index 2: clamps there
        assert_eq!(hover.scroll, 2);
        hover.scroll_up();
        assert_eq!(hover.scroll, 1);
    }
}
