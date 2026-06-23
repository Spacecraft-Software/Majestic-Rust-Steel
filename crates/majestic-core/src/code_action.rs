// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The LSP code-actions menu (PRD #1 §6.9; `textDocument/codeAction`).
//!
//! A popup listing the quick-fixes and refactors the language server offers at the cursor; selecting
//! one applies its edits. Each [`CodeAction`] carries the already-reduced [`RenameEdit`]s of its
//! `WorkspaceEdit` — empty for a command-only action, which v1 cannot apply (it shows a notice
//! instead). Editor-facing only: the `lsp-types` payloads are reduced before they reach here, so the
//! core carries no LSP dependency. Mirrors the references/symbols list popups.

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::finder::draw_box;
use crate::rename::RenameEdit;

/// A code action's command (LSP `workspace/executeCommand`): its identifier and JSON arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The command identifier the server registered.
    pub id: String,
    /// The opaque JSON arguments the server attached to the command.
    pub arguments: Vec<serde_json::Value>,
}

/// One offered code action: a human title plus the edits applying it performs and/or a command to
/// run. An action carrying edits applies them directly; an edit-less action runs its command (the
/// server then sends back the edits via `workspace/applyEdit`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeAction {
    /// The action's title, shown in the menu (e.g. "Import `std::io::Write`").
    pub title: String,
    /// The edits applying this action performs, already reduced from the server's `WorkspaceEdit`.
    /// Empty for a command-only action.
    pub edits: Vec<RenameEdit>,
    /// The command to run when the action has no inline edits. `None` for a pure-edit action.
    pub command: Option<Command>,
}

impl CodeAction {
    /// Creates a code action with `title` that applies `edits` (and no command).
    #[must_use]
    pub fn new(title: impl Into<String>, edits: Vec<RenameEdit>) -> Self {
        Self {
            title: title.into(),
            edits,
            command: None,
        }
    }

    /// Attaches a command to run (for an action whose effect is a `workspace/executeCommand`).
    #[must_use]
    pub fn with_command(mut self, command: Command) -> Self {
        self.command = Some(command);
        self
    }

    /// Whether selecting this action does something — applies an edit or runs a command.
    #[must_use]
    pub fn is_applicable(&self) -> bool {
        !self.edits.is_empty() || self.command.is_some()
    }
}

/// A code-actions menu: the offered actions plus the current selection.
#[derive(Clone, Debug)]
pub struct CodeActions {
    items: Vec<CodeAction>,
    selected: usize,
}

/// Largest popup the menu is allowed to grow to (rows of actions, columns of width).
const MAX_ROWS: u16 = 10;
const MAX_WIDTH: u16 = 60;
const MIN_WIDTH: u16 = 20;

impl CodeActions {
    /// Builds a menu over `items` with the first action selected.
    #[must_use]
    pub fn new(items: Vec<CodeAction>) -> Self {
        Self { items, selected: 0 }
    }

    /// Whether there is nothing to show (the host should not open an empty menu).
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

    /// The currently selected action.
    #[must_use]
    pub fn selected(&self) -> Option<&CodeAction> {
        self.items.get(self.selected)
    }

    /// Draws the menu anchored at `cursor` (an absolute screen position), clamped inside `area`.
    /// Prefers to open just below the cursor, flipping above it when there is no room. Mirrors the
    /// references popup: a bordered box titled `code actions (N)`, the selection inverted.
    pub fn render(&self, surface: &mut Surface, area: Rect, cursor: (u16, u16), theme: &Theme) {
        if self.items.is_empty() || area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let rows = MAX_ROWS.min(u16::try_from(self.items.len()).unwrap_or(MAX_ROWS));
        let height = rows + 2; // a bordered box adds a row top and bottom
        let label_width = self
            .items
            .iter()
            .map(|item| item.title.chars().count())
            .max()
            .unwrap_or(0);
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
            &format!("code actions ({})", self.items.len()),
        );
        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);
        let start = self.scroll_start(usize::from(inner.height));
        let max_chars = usize::from(inner.width);
        for row in 0..inner.height {
            let Some(item) = self.items.get(start + usize::from(row)) else {
                break;
            };
            let selected = start + usize::from(row) == self.selected;
            let style = if selected {
                Style::new(theme.background, theme.accent)
            } else {
                fg
            };
            let y = inner.y + row;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', style);
            }
            // Clip an over-long title so it cannot overwrite the right border.
            let title: String = item.title.chars().take(max_chars).collect();
            surface.set_str(inner.x, y, &title, style);
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

#[cfg(test)]
mod tests {
    use super::{CodeAction, CodeActions};

    #[test]
    fn selection_navigates_and_reports_applicability() {
        let mut menu = CodeActions::new(vec![
            CodeAction::new("editless command", Vec::new()),
            CodeAction::new("a fix", Vec::new()),
        ]);
        assert!(!menu.selected().unwrap().is_applicable()); // no edits
        menu.select_down();
        assert_eq!(menu.selected().unwrap().title, "a fix");
        menu.select_down();
        assert_eq!(menu.selected().unwrap().title, "a fix"); // saturates
    }

    #[test]
    fn empty_menu_reports_empty() {
        assert!(CodeActions::new(Vec::new()).is_empty());
    }
}
