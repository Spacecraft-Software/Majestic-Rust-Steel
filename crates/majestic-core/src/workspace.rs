// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Workspace`] — multiple open buffers arranged into a window tree with a tab bar (PRD #1
//! §6: majestic-core owns buffers and windows; UI.md §3 editor area).
//!
//! Each open buffer is an [`Editor`] (its own viewport, highlighter, and cursor). The editor area
//! is a **binary split tree**: every node is either a leaf (one pane showing one buffer) or a
//! split (an axis + a ratio + two child nodes), so panes nest into arbitrary grids and each split
//! is independently resizable. Exactly one leaf is focused; a tab bar lists every open buffer.
//! Window keys split the focused pane (either axis), move focus, resize, cycle the focused pane
//! through background tabs, and close panes. One shared clipboard is mirrored across panes.

use keymaker::{KeyCode, KeyPress, Mods, Profile};
use penumbra::{Buffer as Surface, Rect, Style, Theme};

use std::io;
use std::ops::Range;
use std::path::Path;

use crate::buffer::Buffer;
use crate::diagnostic::Diagnostic;
use crate::editor::Editor;
use crate::fold::FoldRange;
use crate::inlay::InlayHint;
use crate::occurrence::Occurrence;
use crate::session::{LayoutNode, PaneState, Session};
use crate::whichkey::WhichKey;

/// The axis a split divides its two children along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Split {
    /// Children side by side, separated by a vertical rule (`│`).
    Columns,
    /// Children stacked top to bottom, separated by a horizontal rule (`─`).
    Rows,
}

/// A node of the window tree: a single pane, or a split of two child nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    /// A pane showing the buffer at this editor index.
    Leaf(usize),
    /// A split: `ratio` is the percent of the span given to `first` (the rest, less a divider
    /// cell, goes to `second`).
    Split {
        dir: Split,
        ratio: u16,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// The editor index of the `n`th leaf (in-order), if any.
    fn nth_editor(&self, n: usize) -> Option<usize> {
        match self {
            Self::Leaf(editor) => (n == 0).then_some(*editor),
            Self::Split { first, second, .. } => {
                let left = first.leaf_count();
                if n < left {
                    first.nth_editor(n)
                } else {
                    second.nth_editor(n - left)
                }
            }
        }
    }

    /// Points the `n`th leaf at editor `editor`.
    fn set_nth_editor(&mut self, n: usize, editor: usize) {
        match self {
            Self::Leaf(slot) => {
                if n == 0 {
                    *slot = editor;
                }
            }
            Self::Split { first, second, .. } => {
                let left = first.leaf_count();
                if n < left {
                    first.set_nth_editor(n, editor);
                } else {
                    second.set_nth_editor(n - left, editor);
                }
            }
        }
    }

    /// Collects every leaf's editor index, in-order.
    fn collect_editors(&self, out: &mut Vec<usize>) {
        match self {
            Self::Leaf(editor) => out.push(*editor),
            Self::Split { first, second, .. } => {
                first.collect_editors(out);
                second.collect_editors(out);
            }
        }
    }

    /// Tiles `area`, pushing each leaf's `(editor, rect)` in-order and each split's divider.
    fn layout(
        &self,
        area: Rect,
        panes: &mut Vec<(usize, Rect)>,
        dividers: &mut Vec<(Split, Rect)>,
    ) {
        match self {
            Self::Leaf(editor) => panes.push((*editor, area)),
            Self::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let (a, divider, b) = split_area(area, *dir, *ratio);
                if let Some(rect) = divider {
                    dividers.push((*dir, rect));
                }
                first.layout(a, panes, dividers);
                second.layout(b, panes, dividers);
            }
        }
    }
}

/// Replaces the `n`th leaf with a `dir` split of itself (first) and a `new` leaf (second).
fn split_leaf(node: &mut Node, n: usize, dir: Split, new: usize) {
    match node {
        Node::Leaf(editor) => {
            let editor = *editor;
            *node = Node::Split {
                dir,
                ratio: 50,
                first: Box::new(Node::Leaf(editor)),
                second: Box::new(Node::Leaf(new)),
            };
        }
        Node::Split { first, second, .. } => {
            let left = first.leaf_count();
            if n < left {
                split_leaf(first, n, dir, new);
            } else {
                split_leaf(second, n - left, dir, new);
            }
        }
    }
}

/// Removes the `n`th leaf, collapsing its parent split into the surviving sibling. The root leaf
/// (a single pane) is returned unchanged.
fn remove_leaf(node: Node, n: usize) -> Node {
    match node {
        Node::Leaf(editor) => Node::Leaf(editor),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let left = first.leaf_count();
            if n < left {
                if matches!(*first, Node::Leaf(_)) {
                    *second
                } else {
                    Node::Split {
                        dir,
                        ratio,
                        first: Box::new(remove_leaf(*first, n)),
                        second,
                    }
                }
            } else if matches!(*second, Node::Leaf(_)) {
                *first
            } else {
                Node::Split {
                    dir,
                    ratio,
                    first,
                    second: Box::new(remove_leaf(*second, n - left)),
                }
            }
        }
    }
}

/// Adjusts the ratio of the split directly above the `n`th leaf, growing or shrinking that pane.
fn resize_leaf(node: &mut Node, n: usize, grow: bool) {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        let left = first.leaf_count();
        if n < left {
            if matches!(**first, Node::Leaf(_)) {
                *ratio = adjust(*ratio, grow); // grow the first child → larger ratio
            } else {
                resize_leaf(first, n, grow);
            }
        } else if matches!(**second, Node::Leaf(_)) {
            *ratio = adjust(*ratio, !grow); // grow the second child → smaller ratio
        } else {
            resize_leaf(second, n - left, grow);
        }
    }
}

/// Nudges a split ratio by a fixed step, clamped to a sane `10..=90` so neither pane vanishes.
fn adjust(ratio: u16, increase: bool) -> u16 {
    const STEP: u16 = 6;
    if increase {
        (ratio + STEP).min(90)
    } else {
        ratio.saturating_sub(STEP).max(10)
    }
}

/// A set of open buffers shown as tabs, with the editor area as a binary window tree.
#[derive(Debug)]
pub struct Workspace {
    /// Every open buffer; never removed in v1, so indices stay stable.
    editors: Vec<Editor>,
    /// The window tree over [`Self::editors`].
    root: Node,
    /// In-order position of the focused leaf.
    focused: usize,
    /// Shared clipboard, mirrored into every editor so copy/paste crosses panes.
    clipboard: String,
    /// Indent width (columns) applied to every editor, including newly opened ones.
    tab_width: usize,
    /// Keybinding profile applied to every editor, including newly opened ones.
    profile: Profile,
    /// Latched once a quit command is issued.
    quit: bool,
}

impl Workspace {
    /// Creates a workspace with a single buffer in a single pane.
    #[must_use]
    pub fn new(editor: Editor) -> Self {
        Self::from_editors(vec![editor])
    }

    /// Builds a workspace from one or more open buffers; the first is shown in the sole pane, the
    /// rest are background tabs. An empty input becomes a single scratch buffer.
    #[must_use]
    pub fn from_editors(mut editors: Vec<Editor>) -> Self {
        if editors.is_empty() {
            editors.push(Editor::new());
        }
        Self {
            editors,
            root: Node::Leaf(0),
            focused: 0,
            clipboard: String::new(),
            tab_width: 4, // matches Editor's default; overridden by `set_tab_width` from config
            profile: Profile::Cua, // matches Editor's default; overridden by `set_profile`
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

    /// Applies `diagnostics` to every pane showing the file at `path` (so both views of one file
    /// update together). Called from the host with the language server's published diagnostics.
    pub fn apply_diagnostics(&mut self, path: &Path, diagnostics: &[Diagnostic]) {
        for editor in &mut self.editors {
            if editor.buffer().path().as_deref() == Some(path) {
                editor.set_diagnostics(diagnostics.to_vec());
            }
        }
    }

    /// Applies inlay `hints` to every pane showing the file at `path` (so both views update
    /// together). Called from the host with the language server's `textDocument/inlayHint` reply.
    pub fn apply_inlay_hints(&mut self, path: &Path, hints: &[InlayHint]) {
        for editor in &mut self.editors {
            if editor.buffer().path().as_deref() == Some(path) {
                editor.set_inlay_hints(hints.to_vec());
            }
        }
    }

    /// Applies foldable `folds` to every pane showing the file at `path` (the ranges are per-file;
    /// each pane keeps its own collapsed state). From the server's `textDocument/foldingRange` reply.
    pub fn apply_folds(&mut self, path: &Path, folds: &[FoldRange]) {
        for editor in &mut self.editors {
            if editor.buffer().path().as_deref() == Some(path) {
                editor.set_folds(folds.to_vec());
            }
        }
    }

    /// Toggles the fold under the cursor in the focused pane (the `toggle-fold` command).
    pub fn toggle_active_fold(&mut self) {
        self.active_mut().toggle_fold();
    }

    /// Captures the layout, open files, and cursor/viewport of every pane into a [`Session`] for
    /// persistence. Unsaved scratch buffers are recorded with no path (they restore empty).
    #[must_use]
    pub fn to_session(&self) -> Session {
        let mut panes = Vec::new();
        let layout = self.node_to_layout(&self.root, &mut panes);
        Session {
            panes,
            layout,
            focused: self.focused,
        }
    }

    /// Rebuilds a workspace from a saved [`Session`]: reopens each pane's file (a scratch pane, or
    /// a file that no longer opens, becomes an empty buffer) and restores the layout, focus, and
    /// cursor/viewport. The host applies config (indent width, …) afterward.
    #[must_use]
    pub fn from_session(session: &Session) -> Self {
        let mut editors: Vec<Editor> = session
            .panes
            .iter()
            .map(|pane| {
                let mut editor = match &pane.path {
                    Some(path) => Buffer::open(path.clone())
                        .map_or_else(|_| Editor::new(), Editor::with_buffer),
                    None => Editor::new(),
                };
                editor.restore_position(pane.cursor, pane.viewport_top, pane.viewport_left);
                editor
            })
            .collect();
        if editors.is_empty() {
            editors.push(Editor::new());
        }

        // Trust the layout only if every leaf indexes a real pane; otherwise fall back to a single
        // pane so a corrupt session file degrades gracefully rather than panicking (Stability P1).
        let root = Self::layout_to_node(&session.layout);
        let mut leaves = Vec::new();
        root.collect_editors(&mut leaves);
        let (root, focused) = if !leaves.is_empty() && leaves.iter().all(|&i| i < editors.len()) {
            (root, session.focused.min(leaves.len() - 1))
        } else {
            (Node::Leaf(0), 0)
        };

        Self {
            editors,
            root,
            focused,
            clipboard: String::new(),
            tab_width: 4,
            profile: Profile::Cua,
            quit: false,
        }
    }

    /// Walks the window tree into a serializable [`LayoutNode`], collecting each pane's state into
    /// `panes` and referencing it by its in-order ordinal.
    fn node_to_layout(&self, node: &Node, panes: &mut Vec<PaneState>) -> LayoutNode {
        match node {
            Node::Leaf(index) => {
                let editor = &self.editors[*index];
                let (viewport_top, viewport_left) = editor.viewport();
                panes.push(PaneState {
                    path: editor.buffer().path(),
                    cursor: editor.buffer().cursor(),
                    viewport_top,
                    viewport_left,
                });
                LayoutNode::Leaf(panes.len() - 1)
            }
            Node::Split {
                dir,
                ratio,
                first,
                second,
            } => LayoutNode::Split {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(self.node_to_layout(first, panes)),
                second: Box::new(self.node_to_layout(second, panes)),
            },
        }
    }

    /// Rebuilds the window tree from a serialized [`LayoutNode`] (leaves index `editors` directly,
    /// since `from_session` builds them in ordinal order).
    fn layout_to_node(layout: &LayoutNode) -> Node {
        match layout {
            LayoutNode::Leaf(ordinal) => Node::Leaf(*ordinal),
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => Node::Split {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(Self::layout_to_node(first)),
                second: Box::new(Self::layout_to_node(second)),
            },
        }
    }

    /// Sets the keybinding profile for every open editor and any opened later. Applied from the
    /// `keymap` config field at startup and by the profile-switch commands at runtime; switching
    /// live never drops a keystroke (dispatch is synchronous — see [`Editor::set_profile`]).
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = profile;
        for editor in &mut self.editors {
            editor.set_profile(profile);
        }
    }

    /// The active keybinding profile (applied to every pane).
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The which-key hint for the focused pane's in-progress key prefix, or `None` when no
    /// multi-key sequence is pending. The host renders it over the editor area.
    #[must_use]
    pub fn which_key(&self) -> Option<WhichKey> {
        let rows = self.active().which_key();
        (!rows.is_empty()).then(|| WhichKey::new(rows))
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
        self.root.nth_editor(self.focused).unwrap_or(0)
    }

    /// The number of panes (leaves) in the window tree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.root.leaf_count()
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
        self.active_mut().buffer_mut().insert(text); // one edit (clean undo + sibling propagation)
        self.propagate_edits();
    }

    /// Replaces `range` in the focused buffer with `text` (applying an LSP completion over the
    /// identifier prefix already typed), propagating the edit to the buffer's other views.
    pub fn replace_active(&mut self, range: Range<usize>, text: &str) {
        self.active_mut().buffer_mut().replace_range(range, text);
        self.propagate_edits();
    }

    /// The screen position of the focused pane's cursor, given the same `area` the editor was last
    /// rendered into. Anchors completion/hover popups at the cursor; `None` when it is off-screen.
    #[must_use]
    pub fn active_cursor_screen(&self, area: Rect) -> Option<(u16, u16)> {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.root.layout(area, &mut panes, &mut dividers);
        let (editor_index, rect) = panes.get(self.focused).copied()?;
        self.editors[editor_index].cursor_screen_position(rect)
    }

    /// Runs editor command `command` on the focused pane, propagating any resulting edit to the
    /// other views of the same buffer.
    pub fn execute(&mut self, command: &str) {
        self.active_mut().execute(command);
        self.propagate_edits();
    }

    /// Takes the focused pane's pending `find` request (set when the `find` command runs), clearing
    /// it — so the host can open its search UI. See [`Editor::take_search_requested`].
    pub fn take_search_request(&mut self) -> bool {
        self.active_mut().take_search_requested()
    }

    /// Opens `editor` as a new buffer and shows it in the focused pane (its previous buffer stays
    /// open as a background tab). Used by the explorer and the fuzzy file finder.
    pub fn open(&mut self, mut editor: Editor) {
        editor.set_tab_width(self.tab_width);
        editor.set_profile(self.profile);
        self.editors.push(editor);
        let index = self.editors.len() - 1;
        self.root.set_nth_editor(self.focused, index);
    }

    /// Shows the file at `path` in the focused pane, **reusing an already-open editor** when one
    /// exists — so a goto-definition jump never opens a second, divergent [`Document`] for a file
    /// that is already open, and a same-file jump simply refocuses the active editor. Opens the
    /// file only when it is not yet open anywhere.
    ///
    /// [`Document`]: crate::Buffer
    ///
    /// # Errors
    /// Returns an I/O error only when the file must be opened from disk and cannot be read.
    pub fn reveal_path(&mut self, path: &Path) -> io::Result<()> {
        if let Some(index) = self
            .editors
            .iter()
            .position(|editor| editor.buffer().path().as_deref() == Some(path))
        {
            self.root.set_nth_editor(self.focused, index);
            return Ok(());
        }
        self.open(Editor::with_buffer(Buffer::open(path)?));
        Ok(())
    }

    /// Moves the focused pane's cursor to byte `offset` (clamped to a char boundary). Used to land
    /// on a goto-definition target once its destination file has been revealed.
    pub fn set_active_cursor(&mut self, offset: usize) {
        self.active_mut().buffer_mut().set_cursor(offset);
    }

    /// Tints the given symbol occurrences in the focused pane (LSP `documentHighlight`); the host
    /// refreshes these as the cursor moves to a new identifier.
    pub fn set_active_occurrences(&mut self, occurrences: Vec<Occurrence>) {
        self.active_mut().set_occurrences(occurrences);
    }

    /// Clears the focused pane's occurrence tint (when the cursor leaves an identifier). A no-op when
    /// nothing is tinted, so it is cheap to call every frame.
    pub fn clear_active_occurrences(&mut self) {
        if self.active_mut().has_occurrences() {
            self.active_mut().set_occurrences(Vec::new());
        }
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
        self.propagate_edits();
    }

    /// Propagates each view's pending edit to the other views sharing its document, rebasing
    /// their cursors so two views of one buffer track each other's forward edits.
    fn propagate_edits(&mut self) {
        let ids: Vec<usize> = self
            .editors
            .iter()
            .map(|editor| editor.buffer().document_id())
            .collect();
        let pending: Vec<(usize, _)> = self
            .editors
            .iter_mut()
            .enumerate()
            .filter_map(|(index, editor)| editor.buffer_mut().take_last_edit().map(|e| (index, e)))
            .collect();
        for (source, edit) in pending {
            for index in 0..self.editors.len() {
                if index != source && ids[index] == ids[source] {
                    self.editors[index].buffer_mut().shift_cursor(&edit);
                }
            }
        }
    }

    /// Handles the workspace-level window keys; returns `true` when `key` was one of them.
    ///
    /// Provisional bindings (full Keymaker rebinding lands at M2): `Ctrl+\` split the focused pane
    /// into columns, `Alt+\` into rows; `Alt+o` focus the next pane; `Alt+↑/↓` grow/shrink the
    /// focused pane; `Alt+←/→` previous/next buffer in the focused pane; `Ctrl+W` close the pane.
    fn window_command(&mut self, key: KeyPress) -> bool {
        if key == KeyPress::ctrl('\\') {
            self.split_focused(Split::Columns);
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Char('\\')) {
            self.split_focused(Split::Rows);
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Char('o')) {
            self.focus_next();
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Up) {
            self.resize(true);
        } else if key == KeyPress::new(Mods::ALT, KeyCode::Down) {
            self.resize(false);
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

    /// Splits the focused pane along `dir`, opening a **second view of the same buffer** in the
    /// new pane (Emacs `C-x 2`/`C-x 3`): shared text + undo, independent cursor and scroll. The
    /// new pane becomes focused; `Alt+←/→` re-points it at another buffer if desired.
    fn split_focused(&mut self, dir: Split) {
        let mut view = self.active().view();
        view.set_clipboard(&self.clipboard); // share the kill-ring with the new pane
        self.editors.push(view);
        let next = self.editors.len() - 1;
        split_leaf(&mut self.root, self.focused, dir, next);
        self.focused += 1; // the new leaf is the second child, just after the old one
    }

    /// Grows (or shrinks) the focused pane by adjusting its enclosing split's ratio.
    fn resize(&mut self, grow: bool) {
        resize_leaf(&mut self.root, self.focused, grow);
    }

    /// Editor indices shown across all panes, in-order.
    fn shown_editors(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.root.collect_editors(&mut out);
        out
    }

    /// Moves focus to the next pane (wrapping).
    fn focus_next(&mut self) {
        let count = self.root.leaf_count();
        if count > 0 {
            self.focused = (self.focused + 1) % count;
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
        self.root.set_nth_editor(self.focused, candidates[next]);
    }

    /// Whether buffer `index` is shown in a pane other than the focused one.
    fn shown_in_other_pane(&self, index: usize) -> bool {
        self.shown_editors()
            .iter()
            .enumerate()
            .any(|(pane, &buffer)| pane != self.focused && buffer == index)
    }

    /// Closes the focused pane (the buffer stays open as a tab); a no-op with a single pane.
    fn close_pane(&mut self) {
        if self.root.leaf_count() <= 1 {
            return;
        }
        let root = std::mem::replace(&mut self.root, Node::Leaf(0));
        self.root = remove_leaf(root, self.focused);
        let count = self.root.leaf_count();
        if self.focused >= count {
            self.focused = count - 1;
        }
    }

    /// Draws the tab bar and the window tree into `area`. `focused` is whether the editor area
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
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.root.layout(body, &mut panes, &mut dividers);
        for (position, (index, rect)) in panes.into_iter().enumerate() {
            let pane_focused = focused && position == self.focused;
            self.editors[index].render_in(surface, rect, theme, pane_focused);
        }
        let style = Style::new(theme.accent, theme.background); // Steel Blue rules
        for (dir, rect) in dividers {
            let glyph = match dir {
                Split::Columns => '│',
                Split::Rows => '─',
            };
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    surface.set_char(x, y, glyph, style);
                }
            }
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
        let shown = self.shown_editors();
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
            } else if shown.contains(&index) {
                Style::new(theme.accent, theme.background) // shown in another pane
            } else {
                base // background tab
            };
            x = surface.set_str(x, area.y, &label, style);
        }
    }
}

/// Splits `area` along `dir` at `ratio` percent, returning `(first, divider, second)`. The
/// divider is a one-cell rule between the children (omitted when there is no room for it). Sizes
/// are clamped so each child keeps at least one cell whenever the area can hold both.
fn split_area(area: Rect, dir: Split, ratio: u16) -> (Rect, Option<Rect>, Rect) {
    match dir {
        Split::Columns => {
            if area.width < 2 {
                return (area, None, Rect::new(area.right(), area.y, 0, area.height));
            }
            let with_divider = area.width >= 3;
            let span = if with_divider {
                area.width - 1
            } else {
                area.width
            };
            let first_w = first_size(span, ratio);
            let first = Rect::new(area.x, area.y, first_w, area.height);
            let second_x = area.x + first_w + u16::from(with_divider);
            let divider = with_divider.then(|| Rect::new(area.x + first_w, area.y, 1, area.height));
            let second = Rect::new(second_x, area.y, area.right() - second_x, area.height);
            (first, divider, second)
        }
        Split::Rows => {
            if area.height < 2 {
                return (area, None, Rect::new(area.x, area.bottom(), area.width, 0));
            }
            let with_divider = area.height >= 3;
            let span = if with_divider {
                area.height - 1
            } else {
                area.height
            };
            let first_h = first_size(span, ratio);
            let first = Rect::new(area.x, area.y, area.width, first_h);
            let second_y = area.y + first_h + u16::from(with_divider);
            let divider = with_divider.then(|| Rect::new(area.x, area.y + first_h, area.width, 1));
            let second = Rect::new(area.x, second_y, area.width, area.bottom() - second_y);
            (first, divider, second)
        }
    }
}

/// The first child's size: `ratio` percent of `span`, clamped to `1..=span-1` so both children
/// keep at least one cell.
fn first_size(span: u16, ratio: u16) -> u16 {
    let scaled = u32::from(span) * u32::from(ratio) / 100;
    u16::try_from(scaled).unwrap_or(span).clamp(1, span - 1)
}

#[cfg(test)]
mod tests {
    use super::{first_size, split_area, Node, Split, Workspace};
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use keymaker::{KeyCode, KeyPress, Mods, Profile};
    use penumbra::{Buffer as Surface, Rect, Theme};

    fn alt(code: KeyCode) -> KeyPress {
        KeyPress::new(Mods::ALT, code)
    }

    #[test]
    fn starts_with_a_single_pane_and_tab() {
        let workspace = Workspace::new(Editor::new());
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.active_index(), 0);
        assert!(!workspace.should_quit());
    }

    #[test]
    fn split_opens_a_second_view_of_the_current_buffer() {
        let mut workspace = Workspace::new(Editor::with_buffer(Buffer::from_text("hi")));
        workspace.handle_key(KeyPress::ctrl('\\'));
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.editors.len(), 2); // a second *view*, not a new document
        assert_eq!(workspace.focused, 1); // focus moved to the new pane
                                          // Both panes show the same buffer.
        assert_eq!(workspace.editors[0].buffer().text(), "hi");
        assert_eq!(workspace.editors[1].buffer().text(), "hi");
    }

    #[test]
    fn split_shows_the_current_buffer_not_a_background_tab() {
        // `one` is in the pane; `two` is a background tab. Splitting views `one` again, leaving
        // `two` reachable via Alt+←/→ — it is not pulled into the new pane.
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("one")),
            Editor::with_buffer(Buffer::from_text("two")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        workspace.handle_key(KeyPress::ctrl('\\'));
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.editors.len(), 3); // `one`, `two`, and a new view of `one`
        assert_eq!(workspace.active().buffer().text(), "one");
    }

    #[test]
    fn split_views_share_edits() {
        let mut workspace = Workspace::new(Editor::with_buffer(Buffer::from_text("hi")));
        workspace.handle_key(KeyPress::ctrl('\\')); // focus the new view of the same buffer
        let editor = workspace.active_mut();
        editor.buffer_mut().move_line_end(false);
        editor.self_insert('!');
        // The edit is visible in the first view — they share one document.
        assert_eq!(workspace.editors[0].buffer().text(), "hi!");
        assert_eq!(workspace.editors[1].buffer().text(), "hi!");
    }

    #[test]
    fn split_views_track_each_others_cursors() {
        let mut workspace = Workspace::new(Editor::with_buffer(Buffer::from_text("hello")));
        workspace.active_mut().buffer_mut().move_line_end(false); // pane 0 cursor at byte 5
        workspace.handle_key(KeyPress::ctrl('\\')); // split -> pane 1 (a view, cursor 0), focused
        workspace.handle_key(KeyPress::char('X')); // pane 1 inserts 'X' at the start
        assert_eq!(workspace.editors[0].buffer().text(), "Xhello");
        assert_eq!(
            workspace.editors[0].buffer().cursor(),
            6,
            "pane 0's cursor tracked pane 1's insertion before it (5 -> 6)"
        );
    }

    #[test]
    fn nested_splits_form_a_grid() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\')); // split into columns -> 2 panes
        workspace.handle_key(alt(KeyCode::Char('\\'))); // split the focused pane into rows -> 3
        assert_eq!(workspace.pane_count(), 3);
        // The root is a Columns split whose second child is itself a Rows split (a grid).
        let Node::Split { dir, second, .. } = &workspace.root else {
            panic!("root should be a split");
        };
        assert_eq!(*dir, Split::Columns);
        assert!(matches!(
            **second,
            Node::Split {
                dir: Split::Rows,
                ..
            }
        ));
    }

    #[test]
    fn resize_adjusts_the_enclosing_split_ratio() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\')); // 2 columns, focus on the second pane
        workspace.handle_key(alt(KeyCode::Up)); // grow the focused (second) pane
        let Node::Split { ratio, .. } = &workspace.root else {
            panic!("root should be a split");
        };
        assert!(
            *ratio < 50,
            "growing the second pane lowers the first's ratio: {ratio}"
        );
        workspace.handle_key(alt(KeyCode::Down)); // shrink it back
        let Node::Split { ratio, .. } = &workspace.root else {
            unreachable!()
        };
        assert_eq!(*ratio, 50);
    }

    #[test]
    fn close_pane_keeps_the_buffer_and_collapses_the_split() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.handle_key(KeyPress::ctrl('\\')); // 2 panes, 2 editors
        workspace.handle_key(KeyPress::ctrl('w')); // close focused pane
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.editors.len(), 2); // the buffer survives as a tab
        assert!(matches!(workspace.root, Node::Leaf(_))); // the split collapsed
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
    fn set_profile_reaches_every_pane_and_new_buffers() {
        let mut workspace = Workspace::new(Editor::new());
        workspace.set_profile(Profile::Vim);
        assert_eq!(workspace.profile(), Profile::Vim);
        // The existing pane is now in Vim Normal mode: `i` switches to Insert, then text inserts.
        workspace.handle_key(KeyPress::char('x')); // Normal mode: swallowed
        assert_eq!(workspace.active().buffer().text(), "");
        // A newly opened buffer inherits the workspace profile (still Vim/Normal).
        workspace.open(Editor::new());
        workspace.handle_key(KeyPress::char('x')); // Normal mode: swallowed
        assert_eq!(workspace.active().buffer().text(), "");
        workspace.handle_key(KeyPress::char('i')); // enter Insert
        workspace.handle_key(KeyPress::char('x'));
        assert_eq!(workspace.active().buffer().text(), "x");
    }

    #[test]
    fn clipboard_is_shared_across_panes() {
        let editors = vec![
            Editor::with_buffer(Buffer::from_text("hello")),
            Editor::with_buffer(Buffer::from_text("")),
        ];
        let mut workspace = Workspace::from_editors(editors);
        workspace.handle_key(KeyPress::ctrl('a')); // select-all in buffer "hello"
        workspace.handle_key(KeyPress::ctrl('c')); // copy -> shared clipboard
        workspace.handle_key(KeyPress::ctrl('\\')); // split -> a second view of "hello"
        workspace.handle_key(alt(KeyCode::Right)); // re-point the new pane at the empty buffer
        assert_eq!(workspace.active().buffer().text(), "");
        workspace.handle_key(KeyPress::ctrl('v')); // paste the other pane's copy
        assert_eq!(workspace.active().buffer().text(), "hello");
    }

    #[test]
    fn split_area_columns_reserves_a_divider() {
        // width 10 at 50%: 1 divider, 9 cells split 4 + 5.
        let (first, divider, second) = split_area(Rect::new(0, 0, 10, 4), Split::Columns, 50);
        assert_eq!(first, Rect::new(0, 0, 4, 4));
        assert_eq!(divider, Some(Rect::new(4, 0, 1, 4)));
        assert_eq!(second, Rect::new(5, 0, 5, 4));
    }

    #[test]
    fn first_size_clamps_so_both_children_survive() {
        assert_eq!(first_size(9, 50), 4);
        assert_eq!(first_size(9, 0), 1); // never zero
        assert_eq!(first_size(9, 100), 8); // never the whole span
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

        let tabs: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, 0).map(|c| c.symbol))
            .collect();
        assert!(
            tabs.contains("scratch"),
            "tab bar lists the buffers: {tabs:?}"
        );

        let has_divider = (0..surface.width()).any(|x| {
            (1..surface.height()).any(|y| surface.cell(x, y).is_some_and(|c| c.symbol == '│'))
        });
        assert!(
            has_divider,
            "expected a vertical divider between the two panes"
        );
    }

    #[test]
    fn single_scratch_session_round_trips_to_one_pane() {
        let workspace = Workspace::new(Editor::new());
        let session = workspace.to_session();
        assert_eq!(session.panes.len(), 1);
        assert!(session.panes[0].path.is_none());
        assert_eq!(Workspace::from_session(&session).pane_count(), 1);
    }

    #[test]
    fn session_captures_and_restores_split_layout_and_cursor() {
        // Open a real file (so it reopens on restore), split it, move the cursor, round-trip.
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-ws-session-{}.txt", std::process::id()));
        let mut journal = path.clone().into_os_string();
        journal.push(".mjjournal");
        std::fs::write(&path, "hello world\nsecond line\n").unwrap();

        let mut workspace =
            Workspace::new(Editor::with_buffer(Buffer::open(path.clone()).unwrap()));
        workspace.handle_key(KeyPress::ctrl('\\')); // split into two panes
        workspace.active_mut().execute("move-right");
        workspace.active_mut().execute("move-right");
        let cursor = workspace.active().buffer().cursor();

        let session = workspace.to_session();
        assert_eq!(session.panes.len(), 2);

        let restored = Workspace::from_session(&session);
        assert_eq!(restored.pane_count(), 2);
        assert_eq!(restored.active().buffer().cursor(), cursor);
        assert_eq!(
            restored.active().buffer().text(),
            "hello world\nsecond line\n"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }
}
