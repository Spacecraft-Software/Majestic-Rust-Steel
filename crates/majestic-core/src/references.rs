// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP find-references popup (PRD #1 §6.9).
//!
//! A scrollable list of every place a symbol is used, anchored at the cursor. The host requests
//! references from the language server (off-thread), builds a [`References`] from the result, and
//! draws it over the editor with [`References::render`]; ↑/↓ move the selection and Enter jumps to
//! [`References::selected`] (the destination file + position). Editor-facing only — the `lsp-types`
//! `Location`s are converted to [`Reference`]s (with an off-thread source-line preview) before they
//! reach here, so the core carries no LSP dependency. The companion to goto-definition: where
//! goto-definition jumps to the single declaration, this lists every use.

use std::path::PathBuf;

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// One reference (a single use site) in editor-facing form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The file the reference is in.
    pub path: PathBuf,
    /// The zero-based line of the reference (LSP coordinates) — used both to jump and to show
    /// `file:line` in the list.
    pub line: u32,
    /// The zero-based character offset within [`Self::line`] (LSP coordinates) — used to land the
    /// cursor precisely on the symbol when jumping.
    pub character: u32,
    /// The trimmed text of the source line, shown after the location as a preview. Empty when the
    /// file could not be read (the location still navigates correctly).
    pub preview: String,
}

impl Reference {
    /// The `name:line` label shown for this reference (the file's name plus its one-based line).
    #[must_use]
    fn location(&self) -> String {
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?");
        format!("{name}:{}", self.line.saturating_add(1))
    }
}

/// A find-references popup: every use site plus the current selection.
#[derive(Clone, Debug)]
pub struct References {
    items: Vec<Reference>,
    selected: usize,
}

/// Largest popup the list is allowed to grow to (rows of references, columns of width). Wider and
/// taller than the completion popup, since a reference row carries a `file:line` plus a code preview.
const MAX_ROWS: u16 = 12;
const MAX_WIDTH: u16 = 64;
const MIN_WIDTH: u16 = 20;

impl References {
    /// Builds a popup over `items` with the first reference selected.
    #[must_use]
    pub fn new(items: Vec<Reference>) -> Self {
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

    /// The currently selected reference (its destination file + position).
    #[must_use]
    pub fn selected(&self) -> Option<&Reference> {
        self.items.get(self.selected)
    }

    /// Draws the popup anchored at `cursor` (an absolute screen position), clamped inside `area`.
    /// Prefers to open just below the cursor, flipping above it when there is no room. Mirrors the
    /// completion popup: a bordered box titled `references (N)`, the selection inverted, with the
    /// list scrolled so the selection stays in view.
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
            &format!("references ({})", self.items.len()),
        );
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        let preview_style = Style::new(theme.accent, theme.background);
        let start = self.scroll_start(usize::from(inner.height));
        for row in 0..inner.height {
            let Some(item) = self.items.get(start + usize::from(row)) else {
                break;
            };
            let selected = start + usize::from(row) == self.selected;
            let (location_style, dim) = if selected {
                let inv = Style::new(theme.background, theme.accent);
                (inv, inv)
            } else {
                (fg, preview_style)
            };
            let y = inner.y + row;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', location_style);
            }
            let after = surface.set_str(inner.x, y, &item.location(), location_style);
            if !item.preview.is_empty() && after + 1 < inner.right() {
                let preview = format!(" {}", item.preview);
                surface.set_str(after + 1, y, &preview, dim);
            }
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

/// The display width a reference row wants: its `file:line` label plus `" preview"` when present.
fn row_width(item: &Reference) -> usize {
    let preview = if item.preview.is_empty() {
        0
    } else {
        item.preview.len() + 1
    };
    item.location().len() + preview
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Reference, References};

    fn reference(name: &str, line: u32, preview: &str) -> Reference {
        Reference {
            path: PathBuf::from(format!("/src/{name}")),
            line,
            character: 0,
            preview: preview.to_owned(),
        }
    }

    #[test]
    fn selection_navigates_and_saturates_at_the_ends() {
        let mut refs = References::new(vec![
            reference("a.rs", 0, "let a = 1;"),
            reference("b.rs", 4, "use a;"),
        ]);
        // Starts on the first; up saturates there.
        assert_eq!(refs.selected().unwrap().path, PathBuf::from("/src/a.rs"));
        refs.select_up();
        assert_eq!(refs.selected().unwrap().path, PathBuf::from("/src/a.rs"));
        // Down advances, then saturates at the last.
        refs.select_down();
        assert_eq!(refs.selected().unwrap().path, PathBuf::from("/src/b.rs"));
        refs.select_down();
        assert_eq!(refs.selected().unwrap().path, PathBuf::from("/src/b.rs"));
    }

    #[test]
    fn location_is_file_name_and_one_based_line() {
        assert_eq!(reference("server.rs", 12, "x").location(), "server.rs:13");
    }

    #[test]
    fn empty_popup_reports_empty() {
        assert!(References::new(Vec::new()).is_empty());
    }
}
