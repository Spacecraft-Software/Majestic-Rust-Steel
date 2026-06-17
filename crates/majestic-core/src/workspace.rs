// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Workspace`] — multiple open buffers arranged into panes with a tab bar (PRD #1 §6:
//! majestic-core owns buffers and windows; UI.md §3 editor area).
//!
//! Each open buffer is an [`Editor`] (its own viewport, highlighter, and cursor). The editor
//! area is divided into one or more *panes* along a single [`Split`] axis; each pane shows one
//! buffer, exactly one pane is focused, and a tab bar lists every open buffer. Window keys
//! split the focused pane, move focus, cycle the focused pane through background tabs, and
//! close panes. One shared clipboard is mirrored across panes so copy/cut/paste crosses them.
//!
//! Nested (grid) splits and two views of one buffer are deferred: v1 keeps one buffer per pane
//! along a single axis, which already covers the daily-driver side-by-side and tabbed workflow.

use keymaker::{KeyCode, KeyPress, Mods};
use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::editor::Editor;

/// The axis a [`Workspace`] divides its panes along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    /// Panes side by side, separated by vertical rules (`│`).
    Columns,
    /// Panes stacked top to bottom, separated by horizontal rules (`─`).
    Rows,
}

/// A set of open buffers shown as tabs, with the editor area split into panes.
#[derive(Debug)]
pub struct Workspace {
    /// Every open buffer; never removed in v1, so indices stay stable.
    editors: Vec<Editor>,
    /// The editor index shown in each pane, in layout order. Non-empty and distinct.
    panes: Vec<usize>,
    /// The axis panes are divided along.
    split: Split,
    /// Index into [`Self::panes`] of the focused pane.
    focused: usize,
    /// Shared clipboard, mirrored into every editor so copy/paste crosses panes.
    clipboard: String,
    /// Indent width (columns) applied to every editor, including newly opened ones.
    tab_width: usize,
    /// Latched once a quit command is issued.
    quit: bool,
}

impl Workspace {
    /// Creates a workspace with a single buffer in a single pane.
    #[must_use]
    pub fn new(editor: Editor) -> Self {
        Self::from_editors(vec![editor])
    }

    /// Builds a workspace from one or more open buffers; the first is shown in the sole pane,
    /// the rest are background tabs. An empty input becomes a single scratch buffer.
    #[must_use]
    pub fn from_editors(mut editors: Vec<Editor>) -> Self {
        if editors.is_empty() {
            editors.push(Editor::new());
        }
        Self {
            editors,
            panes: vec![0],
            split: Split::Columns,
            focused: 0,
            clipboard: String::new(),
            tab_width: 4, // matches Editor's default; overridden by `set_tab_width` from config
            quit: false,
        }
    }

    /// Sets the indent width for every open editor and any opened later (applied from config).
    pub fn set_tab_width(&mut self, width: usize) {
        self.tab_width = width;
        for editor in &mut self.editors {
            editor.set_tab_width(width);
        }
    }

    /// Sets the focused editor's status-line message (e.g. a startup notice from the host).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.active_mut().set_status(message);
    }

    /// Whether a quit command has been issued.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The focused editor.
    #[must_use]
    pub fn active(&self) -> &Editor {
        &self.editors[self.active_index()]
    }

    /// The focused editor, mutably.
    pub fn active_mut(&mut self) -> &mut Editor {
        let index = self.active_index();
        &mut self.editors[index]
    }

    fn active_index(&self) -> usize {
        self.panes[self.focused]
    }

    /// The focused buffer's status line, with its tab position appended.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "{}   [{}/{}]",
            self.active().status_line(),
            self.active_index() + 1,
            self.editors.len(),
        )
    }

    /// Inserts `text` into the focused buffer (used for bracketed paste).
    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.active_mut().self_insert(ch);
        }
    }

    /// Opens `editor` as a new buffer and shows it in the focused pane (its previous buffer
    /// stays open as a background tab). Used by the explorer and the fuzzy file finder.
    pub fn open(&mut self, mut editor: Editor) {
        editor.set_tab_width(self.tab_width);
        self.editors.push(editor);
        self.panes[self.focused] = self.editors.len() - 1;
    }

    /// Feeds a key: runs a window command, or forwards it to the focused editor.
    pub fn handle_key(&mut self, key: KeyPress) {
        if self.window_command(key) {
            return;
        }
        let index = self.active_index();
        self.editors[index].handle_key(key);
        if self.editors[index].should_quit() {
            self.quit = true;
        }
        self.sync_clipboard(index);
    }

    /// Handles the workspace-level window keys; returns `true` when `key` was one of them.
    ///
    /// Provisional bindings (full Keymaker rebinding lands at M2): `Ctrl+\` split the focused
    /// pane, `Alt+o` focus the next pane, `Alt+←/→` previous/next buffer in the focused pane,
    /// `Ctrl+W` close the focused pane.
    fn window_command(&mut self, key: KeyPress) -> bool {
        if key == KeyPress::ctrl('\\') {
            self.split_focused();
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Char('o')) {
            self.focus_next();
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Right) {
            self.cycle_buffer(true);
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Left) {
            self.cycle_buffer(false);
        } else if key == KeyPress::ctrl('w') {
            self.close_pane();
        } else {
            return false;
        }
        true
    }

    /// Mirrors the clipboard from editor `index` to all panes (after a copy/cut).
    fn sync_clipboard(&mut self, index: usize) {
        if self.editors[index].clipboard() == self.clipboard {
            return;
        }
        self.clipboard = self.editors[index].clipboard().to_owned();
        for editor in &mut self.editors {
            editor.set_clipboard(&self.clipboard);
        }
    }

    /// Splits the focused pane, showing a not-yet-visible buffer (or a fresh scratch) beside it.
    fn split_focused(&mut self) {
        let next = self.hidden_buffer().unwrap_or_else(|| {
            self.editors.push(Editor::new());
            self.editors.len() - 1
        });
        self.focused += 1;
        self.panes.insert(self.focused, next);
    }

    /// The first open buffer not currently shown in any pane, if any.
    fn hidden_buffer(&self) -> Option<usize> {
        (0..self.editors.len()).find(|index| !self.panes.contains(index))
    }

    /// Moves focus to the next pane (wrapping).
    fn focus_next(&mut self) {
        if !self.panes.is_empty() {
            self.focused = (self.focused + 1) % self.panes.len();
        }
    }

    /// Points the focused pane at the next/previous buffer not shown elsewhere (wrapping).
    fn cycle_buffer(&mut self, forward: bool) {
        let current = self.active_index();
        let candidates: Vec<usize> = (0..self.editors.len())
            .filter(|&index| index == current || !self.shown_in_other_pane(index))
            .collect();
        let len = candidates.len();
        if len <= 1 {
            return;
        }
        let position = candidates
            .iter()
            .position(|&index| index == current)
            .unwrap_or(0);
        let next = if forward {
            (position + 1) % len
        } else {
            (position + len - 1) % len
        };
        self.panes[self.focused] = candidates[next];
    }

    /// Whether buffer `index` is shown in a pane other than the focused one.
    fn shown_in_other_pane(&self, index: usize) -> bool {
        self.panes
            .iter()
            .enumerate()
            .any(|(pane, &buffer)| pane != self.focused && buffer == index)
    }

    /// Closes the focused pane (the buffer stays open as a tab); a no-op with a single pane.
    fn close_pane(&mut self) {
        if self.panes.len() <= 1 {
            return;
        }
        self.panes.remove(self.focused);
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len() - 1;
        }
    }

    /// Draws the tab bar and the pane splits into `area`. `focused` is whether the editor area
    /// (vs. the terminal panel) holds the application's focus.
    pub fn render(&mut self, surface: &mut Surface, area: Rect, theme: &Theme, focused: bool) {
        if area.is_empty() {
            return;
        }
        let (tabs, body) = area.split_top(1);
        self.draw_tab_bar(surface, tabs, theme, focused);
        self.render_panes(surface, body, theme, focused);
    }

    fn render_panes(&mut self, surface: &mut Surface, body: Rect, theme: &Theme, focused: bool) {
        if body.is_empty() {
            return;
        }
        if let Some(rects) = pane_rects(body, self.split, self.panes.len()) {
            draw_dividers(surface, body, self.split, &rects, theme);
            for (pane, rect) in rects.iter().enumerate() {
                let index = self.panes[pane];
                let pane_focused = focused && pane == self.focused;
                self.editors[index].render_in(surface, *rect, theme, pane_focused);
            }
        } else {
            // Too small to tile every pane: show just the focused one full-area.
            let index = self.active_index();
            self.editors[index].render_in(surface, body, theme, focused);
        }
    }

    fn draw_tab_bar(&self, surface: &mut Surface, area: Rect, theme: &Theme, focused: bool) {
        if area.is_empty() {
            return;
        }
        let base = Style::new(theme.foreground, theme.background); // Molten Amber title row
        for x in area.x..area.right() {
            surface.set_char(x, area.y, ' ', base);
        }
        let active = self.active_index();
        let mut x = area.x;
        for (index, editor) in self.editors.iter().enumerate() {
            if x >= area.right() {
                break;
            }
            let dirty = if editor.buffer().is_dirty() {
                "●"
            } else {
                ""
            };
            let label = format!(" {}{dirty} ", editor.display_name());
            let style = if index == active && focused {
                Style::new(theme.background, theme.accent) // active tab, area focused
            } else if index == active {
                Style::new(theme.background, theme.foreground) // active tab, area unfocused
            } else if self.panes.contains(&index) {
                Style::new(theme.accent, theme.background) // shown in another pane
            } else {
                base // background tab
            };
            x = surface.set_str(x, area.y, &label, style);
        }
    }
}

/// The rectangle for each of `count` panes tiling `area` along `split`, with 1-cell dividers
/// between them — or `None` if `area` is too small to give every pane at least one cell.
fn pane_rects(area: Rect, split: Split, count: usize) -> Option<Vec<Rect>> {
    let count = u16::try_from(count).ok()?;
    if count == 0 {
        return None;
    }
    let dividers = count - 1;
    let (mut cursor, span) = match split {
        Split::Columns => (area.x, area.width.checked_sub(dividers)?),
        Split::Rows => (area.y, area.height.checked_sub(dividers)?),
    };
    if span < count {
        return None;
    }
    let base = span / count;
    let extra = span % count;
    let mut rects = Vec::with_capacity(usize::from(count));
    for i in 0..count {
        let size = base + u16::from(i < extra);
        rects.push(match split {
            Split::Columns => Rect::new(cursor, area.y, size, area.height),
            Split::Rows => Rect::new(area.x, cursor, area.width, size),
        });
        cursor = cursor.saturating_add(size);
        if i + 1 < count {
            cursor = cursor.saturating_add(1); // skip the divider cell
        }
    }
    Some(rects)
}

/// Draws the 1-cell rules between panes in the Steelbore accent (Steel Blue).
fn draw_dividers(surface: &mut Surface, area: Rect, split: Split, rects: &[Rect], theme: &Theme) {
    let style = Style::new(theme.accent, theme.background);
    let last = rects.len().saturating_sub(1);
    for rect in rects.iter().take(last) {
        match split {
            Split::Columns => {
                let x = rect.right();
                for y in area.y..area.bottom() {
                    surface.set_char(x, y, '│', style);
                }
            }
            Split::Rows => {
                let y = rect.bottom();
                for x in area.x..area.right() {
                    surface.set_char(x, y, '─', style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{pane_rects, Split, Workspace};
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use keymaker::{KeyCode, KeyPress, Mods};
    use penumbra::{Buffer as Surface, Rect, Theme};

    fn alt(code: KeyCode) -> KeyPress {
        KeyPress::new(Mods::ALT, code)
    }

    #[test]
    fn starts_with_a_single_pane_and_tab() {
        let workspace = Workspace::new(Editor::new());
        assert_eq!(workspace.panes, vec![0]);
        assert_eq!(workspace.active_index(), 0);
        assert!(!workspace.should_quit());
    }

    #[test]
    fn split_with_one_buffer_opens_a_scratch_pane() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\'));
        assert_eq!(workspace.panes.len(), 2);
        assert_eq!(workspace.editors.len(), 2); // a fresh scratch was created
        assert_eq!(workspace.focused, 1); // focus moved to the new pane
    }

    #[test]
    fn split_reuses_a_hidden_buffer() {
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("one")),
            Editor::with_buffer(Buffer::from_text("two")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        assert_eq!(workspace.panes, vec![0]); // editor 1 is a background tab
        workspace.handle_key(KeyPress::ctrl('\\'));
        assert_eq!(workspace.panes, vec![0, 1]); // it became the second pane
        assert_eq!(workspace.editors.len(), 2); // no new scratch needed
        assert_eq!(workspace.active().buffer().text(), "two");
    }

    #[test]
    fn close_pane_keeps_the_buffer() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\')); // 2 panes, 2 editors
        workspace.handle_key(KeyPress::ctrl('w')); // close focused pane
        assert_eq!(workspace.panes.len(), 1);
        assert_eq!(workspace.editors.len(), 2); // the buffer survives as a tab
    }

    #[test]
    fn alt_arrows_cycle_the_focused_pane_through_tabs() {
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("one")),
            Editor::with_buffer(Buffer::from_text("two")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        assert_eq!(workspace.active().buffer().text(), "one");
        workspace.handle_key(alt(KeyCode::Right));
        assert_eq!(workspace.active().buffer().text(), "two");
        workspace.handle_key(alt(KeyCode::Left));
        assert_eq!(workspace.active().buffer().text(), "one");
    }

    #[test]
    fn set_tab_width_reaches_every_pane_and_new_buffers() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.set_tab_width(3);
        workspace.active_mut().execute("indent");
        assert_eq!(workspace.active().buffer().text(), "   ");
        // A buffer opened afterwards inherits the configured width.
        workspace.open(Editor::new());
        workspace.active_mut().execute("indent");
        assert_eq!(workspace.active().buffer().text(), "   ");
    }

    #[test]
    fn window_keys_do_not_reach_the_buffer() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\')); // a window command, not a backslash insert
        assert_eq!(workspace.active().buffer().text(), "");
    }

    #[test]
    fn clipboard_is_shared_across_panes() {
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("hello")),
            Editor::with_buffer(Buffer::from_text("")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        workspace.handle_key(KeyPress::ctrl('a')); // select-all in buffer "one"
        workspace.handle_key(KeyPress::ctrl('c')); // copy -> shared clipboard
        workspace.handle_key(KeyPress::ctrl('\\')); // split: focus the empty second buffer
        assert_eq!(workspace.active().buffer().text(), "");
        workspace.handle_key(KeyPress::ctrl('v')); // paste the other pane's copy
        assert_eq!(workspace.active().buffer().text(), "hello");
    }

    #[test]
    fn pane_rects_tile_columns_with_dividers() {
        // width 10, two columns: 1 divider, 9 cells split 5 + 4.
        let rects = pane_rects(Rect::new(0, 0, 10, 4), Split::Columns, 2).unwrap();
        assert_eq!(rects, vec![Rect::new(0, 0, 5, 4), Rect::new(6, 0, 4, 4)]);
        // The divider sits at column 5 (just past the first pane).
        assert_eq!(rects[0].right(), 5);
    }

    #[test]
    fn pane_rects_bail_when_too_small() {
        // Three columns need ≥3 cells + 2 dividers; width 4 cannot tile them.
        assert!(pane_rects(Rect::new(0, 0, 4, 4), Split::Columns, 3).is_none());
    }

    #[test]
    fn render_draws_tab_bar_and_a_divider() {
        let theme = Theme::steelbore();
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("aaa")),
            Editor::with_buffer(Buffer::from_text("bbb")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        workspace.handle_key(KeyPress::ctrl('\\')); // two side-by-side panes

        let mut surface = Surface::new(40, 8, theme.base_style());
        let area = surface.area();
        workspace.render(&mut surface, area, &theme, true);

        // The tab bar (row 0) lists the open buffers (both unsaved → `[scratch]`).
        let tabs: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, 0).map(|c| c.symbol))
            .collect();
        assert!(
            tabs.contains("scratch"),
            "tab bar should list the buffers: {tabs:?}"
        );

        // A vertical divider appears somewhere in the body.
        let has_divider = (0..surface.width()).any(|x| {
            (1..surface.height()).any(|y| surface.cell(x, y).is_some_and(|c| c.symbol == '│'))
        });
        assert!(
            has_divider,
            "expected a vertical divider between the two panes"
        );
    }
}
