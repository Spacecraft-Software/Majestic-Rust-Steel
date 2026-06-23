// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP document-symbols picker (PRD #1 §6.9).
//!
//! A scrollable outline of every definition in the current file (functions, types, methods, …),
//! anchored at the cursor. The host requests symbols from the language server (off-thread), builds a
//! [`Symbols`] from the result, and draws it over the editor with [`Symbols::render`]; ↑/↓ move the
//! selection and Enter jumps to [`Symbols::selected`] (a position in the same file). Editor-facing
//! only — the `lsp-types` `DocumentSymbol`/`SymbolInformation` payloads are flattened to [`Symbol`]s
//! (with a one-character kind badge and a nesting depth) before they reach here, so the core carries
//! no LSP dependency. The in-file companion to find-references.

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// One symbol (a definition in the current file) in editor-facing form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// The symbol's name (the function, type, or member identifier).
    pub name: String,
    /// A one-character badge for the symbol's kind (e.g. `f` function, `s` struct/class, `t` trait,
    /// `m` module, `e` enum), chosen by the LSP layer from the server's `SymbolKind`.
    pub kind: char,
    /// The zero-based line of the symbol's name (LSP coordinates) — where Enter lands the cursor.
    pub line: u32,
    /// The zero-based character offset of the symbol's name within [`Self::line`] (LSP coordinates).
    pub character: u32,
    /// Nesting depth (0 at the top level); rendered as indentation so members read under their parent.
    pub depth: u16,
}

impl Symbol {
    /// The indented `<badge> name` text shown for this symbol, indented by its nesting depth.
    #[must_use]
    fn row_text(&self) -> String {
        let indent = INDENT_PER_DEPTH * usize::from(self.depth.min(MAX_INDENT_DEPTH));
        format!("{}{} {}", " ".repeat(indent), self.kind, self.name)
    }
}

/// A document-symbols picker: every definition in the file plus the current selection.
#[derive(Clone, Debug)]
pub struct Symbols {
    items: Vec<Symbol>,
    selected: usize,
}

/// Largest popup the list is allowed to grow to (rows of symbols, columns of width).
const MAX_ROWS: u16 = 14;
const MAX_WIDTH: u16 = 56;
const MIN_WIDTH: u16 = 18;
/// Columns of indentation per nesting level, capped at [`MAX_INDENT_DEPTH`] so deeply nested symbols
/// never push their names off the side of the popup.
const INDENT_PER_DEPTH: usize = 2;
const MAX_INDENT_DEPTH: u16 = 6;

impl Symbols {
    /// Builds a picker over `items` with the first symbol selected.
    #[must_use]
    pub fn new(items: Vec<Symbol>) -> Self {
        Self { items, selected: 0 }
    }

    /// Whether there is nothing to show (the host should not open an empty popup).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Moves the selection up one (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the selection down one (saturating at the bottom).
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// The currently selected symbol (its position in the file).
    #[must_use]
    pub fn selected(&self) -> Option<&Symbol> {
        self.items.get(self.selected)
    }

    /// Draws the popup anchored at `cursor` (an absolute screen position), clamped inside `area`.
    /// Prefers to open just below the cursor, flipping above it when there is no room. Mirrors the
    /// references popup: a bordered box titled `symbols (N)`, the selection inverted, the kind badge
    /// dimmed, with the list scrolled so the selection stays in view.
    pub fn render(&self, surface: &mut Surface, area: Rect, cursor: (u16, u16), theme: &Theme) {
        if self.items.is_empty() || area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let rows = MAX_ROWS.min(u16::try_from(self.items.len()).unwrap_or(MAX_ROWS));
        let height = rows + 2; // a bordered box adds a row top and bottom
        let label_width = self.items.iter().map(row_width).max().unwrap_or(0);
        let width = u16::try_from(label_width + 2)
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

        draw_box(
            surface,
            box_area,
            theme,
            &format!("symbols ({})", self.items.len()),
        );
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        let badge_style = Style::new(theme.accent, theme.background);
        let start = self.scroll_start(usize::from(inner.height));
        for row in 0..inner.height {
            let Some(item) = self.items.get(start + usize::from(row)) else {
                break;
            };
            let selected = start + usize::from(row) == self.selected;
            let (name_style, badge) = if selected {
                let inv = Style::new(theme.background, theme.accent);
                (inv, inv)
            } else {
                (fg, badge_style)
            };
            let y = inner.y + row;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', name_style);
            }
            // The indented kind badge (dimmed) then the symbol name (foreground).
            let indent = INDENT_PER_DEPTH * usize::from(item.depth.min(MAX_INDENT_DEPTH));
            let prefix = format!("{}{} ", " ".repeat(indent), item.kind);
            let after = surface.set_str(inner.x, y, &prefix, badge);
            surface.set_str(after, y, &item.name, name_style);
        }
    }

    /// The first visible row so the selection stays in view within `rows` visible rows.
    fn scroll_start(&self, rows: usize) -> usize {
        if rows == 0 || self.selected < rows {
            0
        } else {
            self.selected + 1 - rows
        }
    }
}

/// The display width a symbol row wants: its indented `<badge> name` text.
fn row_width(item: &Symbol) -> usize {
    item.row_text().chars().count()
}

#[cfg(test)]
mod tests {
    use super::{Symbol, Symbols};

    fn symbol(name: &str, kind: char, depth: u16) -> Symbol {
        Symbol {
            name: name.to_owned(),
            kind,
            line: 0,
            character: 0,
            depth,
        }
    }

    #[test]
    fn selection_navigates_and_saturates_at_the_ends() {
        let mut symbols = Symbols::new(vec![
            symbol("LspManager", 's', 0),
            symbol("request_completion", 'f', 1),
        ]);
        assert_eq!(symbols.selected().unwrap().name, "LspManager");
        symbols.select_up();
        assert_eq!(symbols.selected().unwrap().name, "LspManager");
        symbols.select_down();
        assert_eq!(symbols.selected().unwrap().name, "request_completion");
        symbols.select_down();
        assert_eq!(symbols.selected().unwrap().name, "request_completion");
    }

    #[test]
    fn row_text_indents_by_depth_after_the_badge() {
        assert_eq!(symbol("foo", 'f', 0).row_text(), "f foo");
        assert_eq!(symbol("bar", 'f', 1).row_text(), "  f bar");
        // Indentation is capped so deep nesting never runs away.
        assert_eq!(symbol("deep", 'f', 99).row_text(), "            f deep");
    }

    #[test]
    fn empty_picker_reports_empty() {
        assert!(Symbols::new(Vec::new()).is_empty());
    }
}
