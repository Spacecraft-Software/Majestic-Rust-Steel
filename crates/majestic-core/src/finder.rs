// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Finder`] — the fuzzy file picker and command palette (UI.md global UX).
//!
//! A modal overlay: a centred, bordered box holding a query line and a fuzzy-ranked list of
//! candidates. Typing refilters live via [`crate::fuzzy`]; `↑/↓` move the selection; accepting
//! yields the selected [`Action`] for the host to perform (open a file, run a command). The
//! same widget backs both the file finder (`Ctrl+P`) and the command palette (`Ctrl+Shift+P`).

use std::path::{Path, PathBuf};

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::files::collect_files;
use crate::fuzzy::fuzzy_rank;

/// The maximum number of files the picker indexes (bounds the cost on large trees).
const MAX_FILES: usize = 10_000;

/// What accepting a [`Finder`] selection asks the host to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open this file in the editor.
    OpenFile(PathBuf),
    /// Run this editor command by name.
    RunCommand(String),
}

/// One candidate: its display/match text and the action it triggers.
#[derive(Clone, Debug)]
struct Item {
    label: String,
    action: Action,
}

/// A modal fuzzy picker over a fixed set of items.
#[derive(Debug)]
pub struct Finder {
    title: String,
    query: String,
    items: Vec<Item>,
    filtered: Vec<usize>,
    selected: usize,
}

impl Finder {
    /// A file picker over the files under `root` (paths shown relative to it).
    #[must_use]
    pub fn files(root: &Path) -> Self {
        let items = collect_files(root, MAX_FILES)
            .into_iter()
            .map(|path| {
                let label = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                Item {
                    label,
                    action: Action::OpenFile(path),
                }
            })
            .collect();
        Self::new("Open File", items)
    }

    /// A command palette over the named commands.
    #[must_use]
    pub fn commands(names: &[&str]) -> Self {
        let items = names
            .iter()
            .map(|&name| Item {
                label: name.to_owned(),
                action: Action::RunCommand(name.to_owned()),
            })
            .collect();
        Self::new("Command Palette", items)
    }

    fn new(title: &str, items: Vec<Item>) -> Self {
        let mut finder = Self {
            title: title.to_owned(),
            query: String::new(),
            items,
            filtered: Vec::new(),
            selected: 0,
        };
        finder.refilter();
        finder
    }

    /// The current query text.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Appends `c` to the query and refilters.
    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Removes the last query character and refilters.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    /// Moves the selection up one result.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the selection down one result.
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// The action for the current selection, if any.
    #[must_use]
    pub fn accept(&self) -> Option<&Action> {
        let item = *self.filtered.get(self.selected)?;
        Some(&self.items[item].action)
    }

    fn refilter(&mut self) {
        let labels: Vec<&str> = self.items.iter().map(|item| item.label.as_str()).collect();
        self.filtered = fuzzy_rank(&self.query, &labels);
        self.selected = 0;
    }

    /// Draws the picker as a centred modal box over `area`.
    pub fn render(&self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if area.width < 3 || area.height < 3 {
            return; // no room for a bordered box
        }
        let width = area.width.min(64);
        let height = area.height.min(18);
        let box_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        draw_box(surface, box_area, theme, &self.title);

        let inner = Rect::new(
            box_area.x + 1,
            box_area.y + 1,
            box_area.width - 2,
            box_area.height - 2,
        );
        let fg = Style::new(theme.foreground, theme.background);

        // Query line.
        let query_line = format!("> {}\u{2588}", self.query); // trailing block = caret
        surface.set_str(inner.x, inner.y, &query_line, fg);

        // Results below the query line.
        let list_top = inner.y + 1;
        let rows = inner.height.saturating_sub(1);
        if rows == 0 {
            return;
        }
        let start = self.scroll_start(usize::from(rows));
        for i in 0..rows {
            let Some(&item_index) = self.filtered.get(start + usize::from(i)) else {
                break;
            };
            let selected = start + usize::from(i) == self.selected;
            let style = if selected {
                Style::new(theme.background, theme.accent)
            } else {
                fg
            };
            let y = list_top + i;
            for x in inner.x..inner.right() {
                surface.set_char(x, y, ' ', style);
            }
            surface.set_str(inner.x, y, &self.items[item_index].label, style);
        }
    }

    /// The first result index to display so the selection stays visible (bottom-anchored).
    fn scroll_start(&self, rows: usize) -> usize {
        if rows == 0 || self.selected < rows {
            0
        } else {
            self.selected + 1 - rows
        }
    }
}

/// Draws a bordered box (Steel-Blue rules on Void Navy) with `title` on the top edge.
fn draw_box(surface: &mut Surface, area: Rect, theme: &Theme, title: &str) {
    let border = Style::new(theme.accent, theme.background);
    let fill = theme.base_style();
    let (right, bottom) = (area.right() - 1, area.bottom() - 1);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let symbol = match (x, y) {
                _ if x == area.x && y == area.y => '┌',
                _ if x == right && y == area.y => '┐',
                _ if x == area.x && y == bottom => '└',
                _ if x == right && y == bottom => '┘',
                _ if y == area.y || y == bottom => '─',
                _ if x == area.x || x == right => '│',
                _ => ' ',
            };
            let style = if symbol == ' ' { fill } else { border };
            surface.set_char(x, y, symbol, style);
        }
    }
    // Title sits on the top border, e.g. ┤ Open File ├ style — keep it simple: inline label.
    let label = format!(" {title} ");
    surface.set_str(area.x + 2, area.y, &label, border);
}

#[cfg(test)]
mod tests {
    use super::{Action, Finder};
    use penumbra::{Buffer as Surface, Theme};

    #[test]
    fn commands_filter_and_accept() {
        let mut finder = Finder::commands(&["save", "select-all", "split-right", "undo"]);
        // Everything shows before typing.
        assert_eq!(
            finder.accept(),
            Some(&Action::RunCommand("save".to_owned()))
        );

        finder.push('s');
        finder.push('a'); // "sa" -> "save" (consecutive) ranks above "select-all"
        assert_eq!(
            finder.accept(),
            Some(&Action::RunCommand("save".to_owned()))
        );

        finder.select_down();
        assert_eq!(
            finder.accept(),
            Some(&Action::RunCommand("select-all".to_owned()))
        );
    }

    #[test]
    fn typing_a_miss_empties_the_results() {
        let mut finder = Finder::commands(&["save", "undo"]);
        finder.push('z');
        finder.push('z');
        assert_eq!(finder.accept(), None);
        // Backspacing restores matches.
        finder.backspace();
        finder.backspace();
        assert!(finder.accept().is_some());
    }

    #[test]
    fn renders_a_centred_box_with_the_title() {
        let theme = Theme::steelbore();
        let finder = Finder::commands(&["save", "quit"]);
        let mut surface = Surface::new(40, 12, theme.base_style());
        let area = surface.area();
        finder.render(&mut surface, area, &theme);

        let mut text = String::new();
        for y in 0..surface.height() {
            for x in 0..surface.width() {
                if let Some(cell) = surface.cell(x, y) {
                    text.push(cell.symbol);
                }
            }
        }
        assert!(text.contains("Command Palette"), "modal shows its title");
        assert!(text.contains("save"), "modal lists the candidates");
        assert!(text.contains('┌'), "modal has a box border");
    }
}
