// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A built-in **Info / Texinfo reader** (PRD §5.7 invariant, M1) — Emacs-style navigation of
//! GNU `.info` documents.
//!
//! An Info file is a sequence of **nodes** separated by `\u{001f}` (the file-separator control
//! character). Each node opens with a header line — `File: f,  Node: N,  Next: X,  Prev: Y,
//! Up: Z` — followed by the node body, which may contain a `* Menu:` of `* Item::` entries.
//! [`InfoDocument`] parses that structure; [`InfoReader`] drives navigation (next/prev/up,
//! `* Menu:` entries, inline `*note` cross-references, and history) and renders the current node
//! in the Steelbore palette (§9).
//!
//! Cross-file references (`(dir)`, `(other)Node`) are resolved against the opened file's
//! directory and loaded lazily, so navigation spans a whole Info tree. Gzipped `.info.gz`
//! files are not yet decompressed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use penumbra::{Buffer as Surface, Rect, Style, Theme};

/// The Info file-separator control character that delimits nodes.
const NODE_SEPARATOR: char = '\u{001f}';

/// `*note` (the inline cross-reference marker), as a length constant for offset arithmetic.
const NOTE: &str = "*note";

/// A navigable reference within a node: a `* Menu:` entry or an inline `*note …::`
/// cross-reference. Both are selectable and followed with the same key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// Display text (the menu label, or the cross-referenced node name).
    pub label: String,
    /// The in-file node name this reference targets.
    pub target: String,
    /// The body line the reference sits on.
    line: usize,
    /// Character column where the reference starts (0 for whole-line menu entries).
    column: usize,
    /// Character length of the reference span (0 = the whole line, for menu entries).
    length: usize,
    /// `true` for a `* Menu:` entry (highlighted as a whole line); `false` for an inline xref.
    menu: bool,
}

impl Reference {
    /// Parses the text after a leading `* ` (a menu entry) into a reference, or `None`.
    ///
    /// Forms: `Node::  desc` (label == node) or `Label: Node.  desc`. Cross-file targets
    /// (`(file)node`) are kept and resolved by [`InfoReader::goto`].
    fn menu(rest: &str, line: usize) -> Option<Self> {
        let (label, target) = if let Some((node, _description)) = rest.split_once("::") {
            let node = node.trim();
            (node.to_owned(), node.to_owned())
        } else {
            let (label, after) = rest.split_once(": ")?;
            let target = after.split('.').next().unwrap_or_default().trim();
            (label.trim().to_owned(), target.to_owned())
        };
        if target.is_empty() {
            return None;
        }
        Some(Self {
            label,
            target,
            line,
            column: 0,
            length: 0,
            menu: true,
        })
    }
}

/// Finds the next `*note` / `*Note` marker in `slice`, returning its byte offset.
fn find_note(slice: &str) -> Option<usize> {
    let bytes = slice.as_bytes();
    let mut i = 0;
    while i + NOTE.len() <= slice.len() {
        // `*` is ASCII, so it only ever sits on a char boundary — slicing at `i+1` is safe.
        if bytes[i] == b'*' {
            let tail = &slice[i + 1..];
            if tail.starts_with("note") || tail.starts_with("Note") {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Parses the text after `*note␠` into `(label, target, consumed_bytes)`.
///
/// Two forms: `Target::` (label == target) and `Label: Target.`. Cross-file targets
/// (`(file)Node`) are kept verbatim for [`InfoReader::goto`] to resolve.
fn parse_note_body(rest: &str) -> Option<(String, String, usize)> {
    // `Target::` — only when the target itself holds no `:` (which would signal the labelled form).
    if let Some(end) = rest.find("::") {
        let target = rest[..end].trim();
        if !target.is_empty() && !rest[..end].contains(": ") {
            return Some((target.to_owned(), target.to_owned(), end + 2));
        }
    }
    // `Label: Target.`
    let colon = rest.find(": ")?;
    let after = &rest[colon + 2..];
    let dot = after.find('.')?;
    let (label, target) = (rest[..colon].trim(), after[..dot].trim());
    if label.is_empty() || target.is_empty() {
        return None;
    }
    Some((label.to_owned(), target.to_owned(), colon + 2 + dot + 1))
}

/// Appends inline `*note …` cross-references found on `line` to `out`.
fn parse_xrefs(line: &str, line_index: usize, out: &mut Vec<Reference>) {
    let mut search = 0;
    while let Some(found) = find_note(&line[search..]) {
        let start = search + found; // byte offset of the `*`
        let after = &line[start + NOTE.len()..];
        let whitespace = after.len() - after.trim_start().len();
        let parsed = (whitespace > 0).then(|| parse_note_body(&after[whitespace..]));
        if let Some((label, target, consumed)) = parsed.flatten() {
            let span_bytes = NOTE.len() + whitespace + consumed;
            out.push(Reference {
                label,
                target,
                line: line_index,
                column: line[..start].chars().count(),
                length: line[start..start + span_bytes].chars().count(),
                menu: false,
            });
            search = start + span_bytes;
        } else {
            search = start + NOTE.len();
        }
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
    /// Navigable references — `* Menu:` entries and inline `*note` xrefs — in reading order.
    refs: Vec<Reference>,
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
                    "Next" => next = link(value),
                    "Prev" => prev = link(value),
                    "Up" => up = link(value),
                    _ => {}
                }
            }
        }
        let mut refs = parse_menu(&body);
        for (line_index, line) in body.lines().enumerate() {
            parse_xrefs(line, line_index, &mut refs);
        }
        refs.sort_by_key(|reference| (reference.line, reference.column));
        Self {
            name,
            next,
            prev,
            up,
            body,
            refs,
        }
    }
}

/// A navigation link: `None` only for an empty value. Cross-file forms (`(dir)`, `(other)node`)
/// are kept verbatim and resolved by [`InfoReader::goto`].
fn link(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Scans a node body for its `* Menu:` entries (as whole-line references).
fn parse_menu(body: &str) -> Vec<Reference> {
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
            if let Some(entry) = Reference::menu(rest, line_index) {
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

/// The Info-file key for `path`: its base name without a trailing `.info` extension.
fn info_key(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("info");
    name.strip_suffix(".info").unwrap_or(name).to_owned()
}

/// An interactive Info reader: one or more parsed documents (loaded lazily for cross-file
/// navigation), the current node, scroll, reference selection, and a cross-document history.
#[derive(Clone, Debug)]
pub struct InfoReader {
    /// Parsed documents, keyed by Info file name (base name without `.info`).
    docs: HashMap<String, InfoDocument>,
    /// The active document's key.
    file: String,
    /// Directory used to resolve sibling `.info` files for cross-file links (`None` for in-memory
    /// readers, which stay single-file).
    dir: Option<PathBuf>,
    current: usize,
    history: Vec<(String, usize)>,
    scroll: usize,
    ref_selected: usize,
    /// Visible body rows, recorded by [`InfoReader::render`] so selection can scroll into view.
    viewport_rows: usize,
}

impl InfoReader {
    /// Opens and parses an Info file, starting at its `Top` node (or the first node). Sibling
    /// `.info` files in the same directory become reachable through cross-file references.
    ///
    /// # Errors
    /// Returns the underlying I/O error if `path` cannot be read.
    pub fn open(path: &Path) -> io::Result<Self> {
        // Info files are usually UTF-8 but historically Latin-1; read leniently rather than fail.
        let bytes = fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut reader = Self::from_text(&text, info_key(path));
        reader.dir = path.parent().map(Path::to_path_buf);
        Ok(reader)
    }

    /// Builds a single-file reader over already-loaded Info `text`, keyed by `file`.
    #[must_use]
    pub fn from_text(text: &str, file: impl Into<String>) -> Self {
        let file = file.into();
        let doc = InfoDocument::parse(text);
        let current = doc.position("Top").unwrap_or(0);
        let mut docs = HashMap::new();
        docs.insert(file.clone(), doc);
        Self {
            docs,
            file,
            dir: None,
            current,
            history: Vec::new(),
            scroll: 0,
            ref_selected: 0,
            viewport_rows: 0,
        }
    }

    fn doc(&self) -> Option<&InfoDocument> {
        self.docs.get(&self.file)
    }

    /// The current node, if the active document is non-empty.
    #[must_use]
    pub fn node(&self) -> Option<&InfoNode> {
        self.doc().and_then(|doc| doc.nodes.get(self.current))
    }

    /// The active Info file name.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Switches to `(file, index)`, recording history; a no-op when already there.
    fn go_to(&mut self, file: String, index: usize) {
        if file == self.file && index == self.current {
            return;
        }
        self.history.push((self.file.clone(), self.current));
        self.file = file;
        self.current = index;
        self.scroll = 0;
        self.ref_selected = 0;
    }

    /// Follows a reference `target`, resolving cross-file forms (`(file)Node`, `(file)`, `(dir)`)
    /// against the opened file's directory. Returns whether the target was reached.
    pub fn goto(&mut self, target: &str) -> bool {
        if let Some(rest) = target.strip_prefix('(') {
            let (file, node) = match rest.split_once(')') {
                Some((file, node)) => (file, node.trim()),
                None => (rest, ""),
            };
            return self.goto_file(file, node);
        }
        match self.doc().and_then(|doc| doc.position(target)) {
            Some(index) => {
                let file = self.file.clone();
                self.go_to(file, index);
                true
            }
            None => false,
        }
    }

    /// Resolves a cross-file jump: load `file` (if needed) and go to `node` (or its `Top`).
    fn goto_file(&mut self, file: &str, node: &str) -> bool {
        let file = if file.is_empty() {
            self.file.clone()
        } else {
            file.to_owned()
        };
        if !self.load(&file) {
            return false;
        }
        let index = if node.is_empty() {
            self.docs.get(&file).and_then(|doc| doc.position("Top"))
        } else {
            self.docs.get(&file).and_then(|doc| doc.position(node))
        };
        match index {
            Some(index) => {
                self.go_to(file, index);
                true
            }
            None => false,
        }
    }

    /// Loads (and caches) the document for Info file `file` from the opened file's directory.
    fn load(&mut self, file: &str) -> bool {
        if self.docs.contains_key(file) {
            return true;
        }
        let Some(dir) = self.dir.clone() else {
            return false;
        };
        for name in [format!("{file}.info"), file.to_owned()] {
            if let Ok(bytes) = fs::read(dir.join(name)) {
                let text = String::from_utf8_lossy(&bytes);
                self.docs
                    .insert(file.to_owned(), InfoDocument::parse(&text));
                return true;
            }
        }
        false
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

    /// Returns to the previously visited node (cross-document history back).
    pub fn back(&mut self) {
        if let Some((file, index)) = self.history.pop() {
            self.file = file;
            self.current = index;
            self.scroll = 0;
            self.ref_selected = 0;
        }
    }

    /// Follows the currently selected reference (menu entry or inline cross-reference).
    pub fn enter(&mut self) {
        if let Some(target) = self
            .node()
            .and_then(|node| node.refs.get(self.ref_selected))
            .map(|reference| reference.target.clone())
        {
            self.goto(&target);
        }
    }

    /// Selects the previous reference (and scrolls it into view).
    pub fn select_up(&mut self) {
        self.ref_selected = self.ref_selected.saturating_sub(1);
        self.scroll_to_selected();
    }

    /// Selects the next reference (and scrolls it into view).
    pub fn select_down(&mut self) {
        let count = self.node().map_or(0, |node| node.refs.len());
        if self.ref_selected + 1 < count {
            self.ref_selected += 1;
            self.scroll_to_selected();
        }
    }

    /// Scrolls so the selected reference's line is within the (last-rendered) viewport.
    fn scroll_to_selected(&mut self) {
        let Some(line) = self
            .node()
            .and_then(|node| node.refs.get(self.ref_selected))
            .map(|reference| reference.line)
        else {
            return;
        };
        if line < self.scroll {
            self.scroll = line;
        } else if self.viewport_rows > 0 && line >= self.scroll + self.viewport_rows {
            self.scroll = line + 1 - self.viewport_rows;
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

    /// Draws the reader into `area`: a header bar over the scrolling node body. `* Menu:` entries
    /// are highlighted as whole lines and inline `*note` cross-references in place; the selected
    /// reference is inverted (Steelbore palette). Records the body height so selection can scroll.
    pub fn render(&mut self, surface: &mut Surface, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let (header, body_area) = area.split_top(1);
        self.viewport_rows = usize::from(body_area.height);
        let (ref_selected, scroll) = (self.ref_selected, self.scroll);

        let bar = Style::new(theme.background, theme.accent);
        for x in header.x..header.right() {
            surface.set_char(x, header.y, ' ', bar);
        }
        let node = self
            .docs
            .get(&self.file)
            .and_then(|doc| doc.nodes.get(self.current));
        let name = node.map_or("(no nodes)", |node| node.name.as_str());
        let title = format!(
            " Info: {} — {}    n/p/u nav · ↑↓ select · Enter follow · l back · q quit",
            self.file, name
        );
        surface.set_str(header.x, header.y, &clip(&title, header.width), bar);

        let Some(node) = node else {
            return;
        };
        let foreground = Style::new(theme.foreground, theme.background);
        let reference = Style::new(theme.accent, theme.background);
        let selected = Style::new(theme.background, theme.accent);

        for (line_index, line) in node.body.lines().enumerate() {
            // A whole-line menu entry sets the base style for its line.
            let menu_on_line = node
                .refs
                .iter()
                .position(|item| item.menu && item.line == line_index);
            let line_style = match menu_on_line {
                Some(index) if index == ref_selected => selected,
                Some(_) => reference,
                None => foreground,
            };

            if line_index < scroll {
                continue;
            }
            let Ok(row) = u16::try_from(line_index - scroll) else {
                break;
            };
            if row >= body_area.height {
                break;
            }
            let y = body_area.y + row;
            surface.set_str(body_area.x, y, &clip(line, body_area.width), line_style);

            // Overlay inline `*note` cross-references on this line.
            for (index, item) in node.refs.iter().enumerate() {
                if item.menu || item.line != line_index {
                    continue;
                }
                let Ok(column) = u16::try_from(item.column) else {
                    continue;
                };
                if column >= body_area.width {
                    continue;
                }
                let style = if index == ref_selected {
                    selected
                } else {
                    reference
                };
                let text: String = line.chars().skip(item.column).take(item.length).collect();
                surface.set_str(
                    body_area.x + column,
                    y,
                    &clip(&text, body_area.width - column),
                    style,
                );
            }
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
            Some("(dir)"),
            "cross-file links are kept verbatim for goto to resolve"
        );
        assert_eq!(top.refs.len(), 2);
        assert_eq!(top.refs[0].target, "First");
        assert_eq!(top.refs[1].label, "Second");
    }

    #[test]
    fn follows_inline_cross_references() {
        let text = "\u{001f}\n\
File: x.info,  Node: Top,  Up: (dir)\n\
\n\
See *note Other:: for the details, and *note Top:: to return.\n\
\u{001f}\n\
File: x.info,  Node: Other,  Prev: Top,  Up: Top\n\
\n\
The other node.\n";
        let mut reader = InfoReader::from_text(text, "x.info");
        assert_eq!(reader.node().unwrap().name, "Top");

        // Two inline xrefs were parsed (in reading order: Other, then Top).
        assert_eq!(reader.node().unwrap().refs.len(), 2);
        assert!(reader.node().unwrap().refs.iter().all(|r| !r.menu));

        // Enter follows the first reference (`*note Other::`).
        reader.enter();
        assert_eq!(reader.node().unwrap().name, "Other");
    }

    #[test]
    fn parses_labelled_cross_references() {
        let text = "\u{001f}\n\
File: x.info,  Node: Top,  Up: (dir)\n\
\n\
See *note the manual: Other. for the rest.\n\
\u{001f}\n\
File: x.info,  Node: Other,  Up: Top\n\
\n\
Body.\n";
        let mut reader = InfoReader::from_text(text, "x");
        let references = &reader.node().unwrap().refs;
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].label, "the manual");
        assert_eq!(references[0].target, "Other");

        reader.enter();
        assert_eq!(reader.node().unwrap().name, "Other");
    }

    #[test]
    fn follows_cross_file_references() {
        let dir = std::env::temp_dir().join(format!("mj-info-xfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.info"),
            "\u{001f}\nFile: main.info,  Node: Top,  Up: (dir)\n\nSee *note (other)Top:: for more.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("other.info"),
            "\u{001f}\nFile: other.info,  Node: Top,  Up: (dir)\n\nThe other manual.\n",
        )
        .unwrap();

        let mut reader = InfoReader::open(&dir.join("main.info")).unwrap();
        assert_eq!(reader.file(), "main");

        // The inline `*note (other)Top::` loads the sibling file and jumps into it.
        reader.enter();
        assert_eq!(reader.file(), "other");
        assert_eq!(reader.node().unwrap().name, "Top");

        // History back returns across the file boundary.
        reader.back();
        assert_eq!(reader.file(), "main");
        let _ = std::fs::remove_dir_all(&dir);
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
