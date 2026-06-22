// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP completion popup (PRD #1 §6.9).
//!
//! A scrollable list of candidates anchored at the cursor. The host requests completions from the
//! language server (off-thread), builds a [`Completion`] from the result, and draws it over the
//! editor with [`Completion::render`]; ↑/↓ move the selection and Enter inserts
//! [`Completion::selected`] over the identifier prefix already typed. Editor-facing only — the
//! `lsp-types` payloads are converted to [`CompletionItem`] before they reach here, so the core
//! carries no LSP dependency.

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;

/// One completion candidate in editor-facing form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text shown in the list.
    pub label: String,
    /// The text inserted when the item is chosen (replaces the typed prefix).
    pub insert_text: String,
    /// An optional detail (type or signature), shown dimmed after the label.
    pub detail: Option<String>,
}

/// A completion popup: candidates plus the current selection.
#[derive(Clone, Debug)]
pub struct Completion {
    items: Vec<CompletionItem>,
    selected: usize,
}

/// Largest popup the list is allowed to grow to (rows of candidates, columns of width).
const MAX_ROWS: u16 = 10;
const MAX_WIDTH: u16 = 48;
const MIN_WIDTH: u16 = 14;

impl Completion {
    /// Builds a popup over `items` with the first item selected.
    #[must_use]
    pub fn new(items: Vec<CompletionItem>) -> Self {
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

    /// The currently selected candidate.
    #[must_use]
    pub fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    /// Draws the popup anchored at `cursor` (an absolute screen position), clamped inside `area`.
    /// Prefers to open just below the cursor, flipping above it when there is no room.
    pub fn render(&self, surface: &mut Surface, area: Rect, cursor: (u16, u16), theme: &Theme) {
        if self.items.is_empty() || area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let rows = MAX_ROWS.min(u16::try_from(self.items.len()).unwrap_or(MAX_ROWS));
        let height = rows + 2; // a bordered box adds a row top and bottom
        let label_width = self.items.iter().map(display_width).max().unwrap_or(0);
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

        draw_box(surface, box_area, theme, "completion");
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        let detail_style = Style::new(theme.accent, theme.background);
        let start = self.scroll_start(usize::from(inner.height));
        for row in 0..inner.height {
            let Some(item) = self.items.get(start + usize::from(row)) else {
                break;
            };
            let selected = start + usize::from(row) == self.selected;
            let (label_style, dim) = if selected {
                let inv = Style::new(theme.background, theme.accent);
                (inv, inv)
            } else {
                (fg, detail_style)
            };
            let y = inner.y + row;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', label_style);
            }
            let after = surface.set_str(inner.x, y, &item.label, label_style);
            if let Some(detail) = item.detail.as_deref() {
                if after + 1 < inner.right() {
                    let detail = format!(" {detail}");
                    surface.set_str(after + 1, y, &detail, dim);
                }
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

/// The display width a candidate wants: its label plus `" detail"` when present.
fn display_width(item: &CompletionItem) -> usize {
    let detail = item.detail.as_deref().map_or(0, |detail| detail.len() + 1);
    item.label.len() + detail
}
