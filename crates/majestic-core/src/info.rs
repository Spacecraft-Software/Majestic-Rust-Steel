// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A built-in **Info / Texinfo reader** (PRD §5.7 invariant, M1) — Emacs-style navigation of
//! GNU `.info` documents.
//!
//! An Info file is a sequence of **nodes** separated by `\u{001f}` (the file-separator control
//! character). Each node opens with a header line — `File: f,  Node: N,  Next: X,  Prev: Y,
//! Up: Z` — followed by the node body, which may contain a `* Menu:` of `* Item::` entries.
//! [`InfoDocument`] parses that structure; [`InfoReader`] drives navigation (next/prev/up,
//! menu entries, history) and renders the current node in the Steelbore palette (§9).
//!
//! This v1 reads single-file, in-file navigation; cross-file references (`(dir)`, `(other)node`)
//! are shown but not followed. Inline `*note …::` cross-references render as text — following
//! them is a later refinement.

use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

use penumbra::{Buffer as Surface, Rect, Style, Theme};

/// The Info file-separator control character that delimits nodes.
const NODE_SEPARATOR: char = '\u{001f}';

/// One entry of a node's `* Menu:` — a label and the node it jumps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    /// Display text.
    pub label: String,
    /// The in-file node name this entry targets.
    pub target: String,
    /// The entry's line index within the node body (for highlight + scroll-into-view).
    pub line: usize,
}

impl MenuEntry {
    /// Parses the text after a leading `* ` into a menu entry, or `None` if it is not one.
    ///
    /// Forms: `Node::  desc` (label == node) or `Label: Node.  desc`. Cross-file targets
    /// (`(file)node`) are rejected — this v1 navigates within one file.
    fn parse(rest: &str, line: usize) -> Option<Self> {
        if let Some((node, _description)) = rest.split_once("::") {
            let node = node.trim();
            return (!node.is_empty()).then(|| Self {
                label: node.to_owned(),
                target: node.to_owned(),
                line,
            });
        }
        let (label, after) = rest.split_once(": ")?;
        let target = after.split('.').next().unwrap_or_default().trim();
        if target.is_empty() || target.starts_with('(') {
            return None;
        }
        Some(Self {
            label: label.trim().to_owned(),
            target: target.to_owned(),
            line,
        })
    }
}

/// One Info node: its name, navigation links, body text, and parsed menu.
#[derive(Clone, Debug)]
pub struct InfoNode {
    /// The node name (the `Node:` header field).
    pub name: String,
    next: Option<String>,
    prev: Option<String>,
    up: Option<String>,
    /// The node body (everything after the header line).
    pub body: String,
    /// Parsed `* Menu:` entries.
    pub menu: Vec<MenuEntry>,
}

impl InfoNode {
    /// Parses a node from its header line and body.
    fn parse(header: &str, body: String) -> Self {
        let (mut name, mut next, mut prev, mut up) = (String::new(), None, None, None);
        for field in header.split(',') {
            if let Some((key, value)) = field.trim().split_once(": ") {
                let value = value.trim();
                match key {
                    "Node" => value.clone_into(&mut name),
                    "Next" => next = in_file(value),
                    "Prev" => prev = in_file(value),
                    "Up" => up = in_file(value),
                    _ => {}
                }
            }
        }
        let menu = parse_menu(&body);
        Self {
            name,
            next,
            prev,
            up,
            body,
            menu,
        }
    }
}

/// A cross-file ref (`(dir)`, `(other)node`) or empty value yields `None`; an in-file node name
/// yields `Some`.
fn in_file(value: &str) -> Option<String> {
    (!value.is_empty() && !value.starts_with('(')).then(|| value.to_owned())
}

/// Scans a node body for its `* Menu:` entries.
fn parse_menu(body: &str) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    let mut in_menu = false;
    for (line_index, line) in body.lines().enumerate() {
        if line.trim() == "* Menu:" {
            in_menu = true;
            continue;
        }
        if !in_menu {
            continue;
        }
        if let Some(rest) = line.strip_prefix("* ") {
            if let Some(entry) = MenuEntry::parse(rest, line_index) {
                entries.push(entry);
            }
        }
    }
    entries
}

/// A parsed Info document: its nodes in file order, indexed by name.
#[derive(Clone, Debug)]
pub struct InfoDocument {
    nodes: Vec<InfoNode>,
    index: HashMap<String, usize>,
}

impl InfoDocument {
    /// Parses Info `text` into its nodes. Non-node chunks (the preamble, the tag table) are
    /// skipped: a node chunk is one whose first line is a `File: …Node: …` header.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut nodes = Vec::new();
        let mut index = HashMap::new();
        for chunk in text.split(NODE_SEPARATOR) {
            let chunk = chunk.trim_start_matches('\n');
            let mut lines = chunk.splitn(2, '\n');
            let header = lines.next().unwrap_or_default();
            if !header.starts_with("File:") || !header.contains("Node:") {
                continue;
            }
            let body = lines.next().unwrap_or_default().to_owned();
            let node = InfoNode::parse(header, body);
            if !node.name.is_empty() {
                index.entry(node.name.clone()).or_insert(nodes.len());
                nodes.push(node);
            }
        }
        Self { nodes, index }
    }

    /// The number of nodes parsed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the document has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }
}

/// An interactive Info reader: a parsed document plus the current node, scroll, menu selection,
/// and navigation history.
#[derive(Clone, Debug)]
pub struct InfoReader {
    doc: InfoDocument,
    title: String,
    current: usize,
    history: Vec<usize>,
    scroll: usize,
    menu_selected: usize,
}

impl InfoReader {
    /// Opens and parses an Info file, starting at its `Top` node (or the first node).
    ///
    /// # Errors
    /// Returns the underlying I/O error if `path` cannot be read.
    pub fn open(path: &Path) -> io::Result<Self> {
        // Info files are usually UTF-8 but historically Latin-1; read leniently rather than fail.
        let bytes = fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        let title = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("info")
            .to_owned();
        Ok(Self::from_text(&text, title))
    }

    /// Builds a reader over already-loaded Info `text` titled `title`.
    #[must_use]
    pub fn from_text(text: &str, title: impl Into<String>) -> Self {
        let doc = InfoDocument::parse(text);
        let current = doc.position("Top").unwrap_or(0);
        Self {
            doc,
            title: title.into(),
            current,
            history: Vec::new(),
            scroll: 0,
            menu_selected: 0,
        }
    }

    /// The current node, if the document is non-empty.
    #[must_use]
    pub fn node(&self) -> Option<&InfoNode> {
        self.doc.nodes.get(self.current)
    }

    fn go(&mut self, target: usize) {
        if target < self.doc.nodes.len() && target != self.current {
            self.history.push(self.current);
            self.current = target;
            self.scroll = 0;
            self.menu_selected = 0;
        }
    }

    /// Jumps to the in-file node named `name`, returning whether it exists.
    pub fn goto(&mut self, name: &str) -> bool {
        match self.doc.position(name) {
            Some(target) => {
                self.go(target);
                true
            }
            None => false,
        }
    }

    /// Follows the node's `Next` link, if any.
    pub fn next(&mut self) {
        if let Some(name) = self.node().and_then(|node| node.next.clone()) {
            self.goto(&name);
        }
    }

    /// Follows the node's `Prev` link, if any.
    pub fn prev(&mut self) {
        if let Some(name) = self.node().and_then(|node| node.prev.clone()) {
            self.goto(&name);
        }
    }

    /// Follows the node's `Up` link, if any.
    pub fn up(&mut self) {
        if let Some(name) = self.node().and_then(|node| node.up.clone()) {
            self.goto(&name);
        }
    }

    /// Returns to the previously visited node (history back).
    pub fn back(&mut self) {
        if let Some(previous) = self.history.pop() {
            self.current = previous;
            self.scroll = 0;
            self.menu_selected = 0;
        }
    }

    /// Follows the currently selected menu entry.
    pub fn enter(&mut self) {
        if let Some(target) = self
            .node()
            .and_then(|node| node.menu.get(self.menu_selected))
            .map(|entry| entry.target.clone())
        {
            self.goto(&target);
        }
    }

    /// Moves the menu selection up one entry.
    pub fn select_up(&mut self) {
        self.menu_selected = self.menu_selected.saturating_sub(1);
    }

    /// Moves the menu selection down one entry.
    pub fn select_down(&mut self) {
        let count = self.node().map_or(0, |node| node.menu.len());
        if self.menu_selected + 1 < count {
            self.menu_selected += 1;
        }
    }

    /// Scrolls the body down by `rows`.
    pub fn scroll_down(&mut self, rows: usize) {
        let lines = self.node().map_or(0, |node| node.body.lines().count());
        self.scroll = (self.scroll + rows).min(lines.saturating_sub(1));
    }

    /// Scrolls the body up by `rows`.
    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Draws the reader into `area`: a header bar over the scrolling node body, with menu entries
    /// highlighted and the selected one inverted (Steelbore palette).
    pub fn render(&self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let (header, body_area) = area.split_top(1);
        let bar = Style::new(theme.background, theme.accent);
        for x in header.x..header.right() {
            surface.set_char(x, header.y, ' ', bar);
        }
        let name = self.node().map_or("(no nodes)", |node| node.name.as_str());
        let title = format!(
            " Info: {} — {}    n/p/u nav · Enter menu · l back · q quit",
            self.title, name
        );
        surface.set_str(header.x, header.y, &clip(&title, header.width), bar);

        let Some(node) = self.node() else {
            return;
        };
        let foreground = Style::new(theme.foreground, theme.background);
        let menu = Style::new(theme.accent, theme.background);
        let selected = Style::new(theme.background, theme.accent);

        let mut in_menu = false;
        let mut menu_index = 0usize;
        for (line_index, line) in node.body.lines().enumerate() {
            let is_menu_start = line.trim() == "* Menu:";
            let is_entry = in_menu && !is_menu_start && line.starts_with("* ");
            let style = if is_entry {
                let style = if menu_index == self.menu_selected {
                    selected
                } else {
                    menu
                };
                menu_index += 1;
                style
            } else {
                foreground
            };
            if is_menu_start {
                in_menu = true;
            }

            if line_index < self.scroll {
                continue;
            }
            let Ok(row) = u16::try_from(line_index - self.scroll) else {
                break;
            };
            if row >= body_area.height {
                break;
            }
            surface.set_str(
                body_area.x,
                body_area.y + row,
                &clip(line, body_area.width),
                style,
            );
        }
    }
}

/// Truncates `text` to at most `width` characters (Info bodies are effectively ASCII).
fn clip(text: &str, width: u16) -> String {
    text.chars().take(usize::from(width)).collect()
}

#[cfg(test)]
mod tests {
    use super::{InfoDocument, InfoReader};

    /// A three-node Info document with a menu on the Top node.
    const SAMPLE: &str = "This is sample.info, a preamble that is not a node.\n\
\u{001f}\n\
File: sample.info,  Node: Top,  Next: First,  Prev: (dir),  Up: (dir)\n\
\n\
Welcome to the sample manual.\n\
\n\
* Menu:\n\
\n\
* First::       The first chapter.\n\
* Second::      The second chapter.\n\
\u{001f}\n\
File: sample.info,  Node: First,  Next: Second,  Prev: Top,  Up: Top\n\
\n\
This is the first chapter body.\n\
\u{001f}\n\
File: sample.info,  Node: Second,  Prev: First,  Up: Top\n\
\n\
This is the second chapter body.\n\
\u{001f}\n\
Tag Table:\n\
Node: Top\u{007f}123\n";

    #[test]
    fn parses_nodes_links_and_menu() {
        let doc = InfoDocument::parse(SAMPLE);
        assert_eq!(doc.len(), 3, "Top/First/Second — the tag table is skipped");

        let top = &doc.nodes[doc.position("Top").unwrap()];
        assert_eq!(top.name, "Top");
        assert_eq!(top.next.as_deref(), Some("First"));
        assert_eq!(
            top.prev.as_deref(),
            None,
            "(dir) is cross-file, not followed"
        );
        assert_eq!(top.menu.len(), 2);
        assert_eq!(top.menu[0].target, "First");
        assert_eq!(top.menu[1].label, "Second");
    }

    #[test]
    fn navigates_next_prev_up_menu_and_history() {
        let mut reader = InfoReader::from_text(SAMPLE, "sample.info");
        assert_eq!(reader.node().unwrap().name, "Top"); // starts at Top

        reader.next();
        assert_eq!(reader.node().unwrap().name, "First");
        reader.next();
        assert_eq!(reader.node().unwrap().name, "Second");
        reader.prev();
        assert_eq!(reader.node().unwrap().name, "First");
        reader.up();
        assert_eq!(reader.node().unwrap().name, "Top");

        // Menu: select the second entry and follow it.
        reader.select_down();
        reader.enter();
        assert_eq!(reader.node().unwrap().name, "Second");

        // History back unwinds the last jump.
        reader.back();
        assert_eq!(reader.node().unwrap().name, "Top");
    }

    #[test]
    fn empty_document_has_no_node() {
        let reader = InfoReader::from_text("not an info file at all", "x");
        assert!(reader.node().is_none());
    }
}
