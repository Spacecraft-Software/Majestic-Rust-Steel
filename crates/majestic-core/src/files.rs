// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`FileTree`] — the explorer sidebar's model (UI.md §2).
//!
//! A collapsible directory tree rooted at the project directory. Directories are read lazily:
//! the flattened list of visible [`Row`]s is recomputed from the set of expanded directories,
//! so only opened folders are scanned. Unreadable directories are skipped rather than fatal
//! (Priority 1 — never panic on a filesystem error), and dot-entries are hidden in this v1.
//!
//! The widget renders into a Penumbra [`Rect`] in the Steelbore palette and exposes keyboard
//! navigation (`select_up`/`select_down`/`activate`); the host opens the file `activate`
//! returns and routes the rest of the UI.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use penumbra::{Buffer as Surface, Rect, Style, Theme};

use crate::git::{self, GitStatus};

/// One visible line of the tree: a file or directory at a given nesting depth.
#[derive(Clone, Debug)]
struct Row {
    path: PathBuf,
    depth: usize,
    is_dir: bool,
}

/// A collapsible file explorer rooted at one directory.
#[derive(Debug)]
pub struct FileTree {
    root: PathBuf,
    expanded: Vec<PathBuf>,
    rows: Vec<Row>,
    /// Changed paths under `root`, by git status — snapshotted at build/refresh time.
    git: HashMap<PathBuf, GitStatus>,
    selected: usize,
    top: usize,
}

impl FileTree {
    /// Builds a tree rooted at `root`, with the root's children shown (root expanded).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut tree = Self {
            expanded: vec![root.clone()],
            git: git::statuses(&root),
            root,
            rows: Vec::new(),
            selected: 0,
            top: 0,
        };
        tree.rebuild();
        tree
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Moves the selection up one row.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the selection down one row.
    pub fn select_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    /// Activates the selected row: toggles a directory (returning `None`), or returns the file
    /// path to open.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let (path, is_dir) = {
            let row = self.rows.get(self.selected)?;
            (row.path.clone(), row.is_dir)
        };
        if is_dir {
            self.toggle(&path);
            None
        } else {
            Some(path)
        }
    }

    /// Rescans the tree and git status from disk, preserving the expanded set and selection.
    pub fn refresh(&mut self) {
        let selected = self.rows.get(self.selected).map(|row| row.path.clone());
        self.git = git::statuses(&self.root);
        self.rebuild();
        if let Some(path) = selected {
            if let Some(index) = self.rows.iter().position(|row| row.path == path) {
                self.selected = index;
            }
        }
    }

    /// The git status to color a row by: a file's own status, or — for a directory — whether any
    /// changed path lives under it.
    fn status_for(&self, row: &Row) -> Option<GitStatus> {
        if row.is_dir {
            self.git
                .keys()
                .any(|path| path.starts_with(&row.path))
                .then_some(GitStatus::Modified)
        } else {
            self.git.get(&row.path).copied()
        }
    }

    fn toggle(&mut self, path: &Path) {
        if let Some(index) = self.expanded.iter().position(|p| p == path) {
            self.expanded.remove(index);
        } else {
            self.expanded.push(path.to_path_buf());
        }
        let keep = path.to_path_buf();
        self.rebuild();
        if let Some(index) = self.rows.iter().position(|row| row.path == keep) {
            self.selected = index;
        }
    }

    fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.iter().any(|p| p == path)
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        let root = self.root.clone();
        self.append_children(&root, 0);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn append_children(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = read_sorted(dir) else {
            return;
        };
        for (path, is_dir) in entries {
            let descend = is_dir && self.is_expanded(&path);
            self.rows.push(Row {
                path: path.clone(),
                depth,
                is_dir,
            });
            if descend {
                self.append_children(&path, depth + 1);
            }
        }
    }

    /// Draws the explorer into `area`: an `EXPLORER` title row above the scrolling tree.
    pub fn render(&mut self, surface: &mut Surface, area: Rect, theme: &Theme, focused: bool) {
        if area.is_empty() {
            return;
        }
        let (title, list) = area.split_top(1);
        let title_style = Style::new(theme.foreground, theme.background).bold(); // Molten Amber
        for x in title.x..title.right() {
            surface.set_char(x, title.y, ' ', title_style);
        }
        surface.set_str(title.x, title.y, " EXPLORER", title_style);

        if list.is_empty() {
            return;
        }
        self.scroll_into_view(usize::from(list.height));

        for i in 0..list.height {
            let row_index = self.top + usize::from(i);
            let Some(row) = self.rows.get(row_index) else {
                break;
            };
            let y = list.y + i;
            let selected = row_index == self.selected;
            let status = self.status_for(row);
            let style = row_style(row, selected, focused, status, theme);
            for x in list.x..list.right() {
                surface.set_char(x, y, ' ', style);
            }
            let marker = if row.is_dir {
                if self.is_expanded(&row.path) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let label = format!("{}{marker}{}", "  ".repeat(row.depth), file_name(&row.path));
            surface.set_str(list.x, y, &label, style);
        }
    }

    fn scroll_into_view(&mut self, rows_visible: usize) {
        if rows_visible == 0 {
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + rows_visible {
            self.top = self.selected + 1 - rows_visible;
        }
    }
}

/// The style for a tree row. Selected rows take the selection highlight; otherwise the foreground
/// is tinted by git status (Red Oxide = modified/deleted, Radium Green = added/untracked), falling
/// back to Steel Blue for directories and Molten Amber for clean files (UI.md §2).
fn row_style(
    row: &Row,
    selected: bool,
    focused: bool,
    status: Option<GitStatus>,
    theme: &Theme,
) -> Style {
    if selected && focused {
        return Style::new(theme.background, theme.accent);
    }
    if selected {
        return Style::new(theme.background, theme.foreground);
    }
    let fg = match status {
        Some(GitStatus::Modified | GitStatus::Deleted) => theme.error,
        Some(GitStatus::Added | GitStatus::Untracked) => theme.success,
        None if row.is_dir => theme.accent,
        None => theme.foreground,
    };
    Style::new(fg, theme.background)
}

/// Reads `dir`'s entries (skipping dot-entries), sorted directories-first then case-insensitively.
fn read_sorted(dir: &Path) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue; // hide dotfiles in v1
        }
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        entries.push((entry.path(), is_dir));
    }
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1) // directories (true) before files (false)
            .then_with(|| {
                file_name(&a.0)
                    .to_lowercase()
                    .cmp(&file_name(&b.0).to_lowercase())
            })
    });
    Ok(entries)
}

/// The final path component as a string (empty if there is none).
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Collects up to `limit` file paths under `root` (recursive, dot-entries skipped).
///
/// The traversal is bounded by `limit` so a huge tree cannot stall the fuzzy file finder;
/// unreadable directories are skipped rather than fatal. Order is unspecified — the finder
/// ranks results by fuzzy score regardless.
pub fn collect_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if files.len() >= limit {
            break;
        }
        let Ok(entries) = read_sorted(&dir) else {
            continue; // skip directories we cannot read
        };
        for (path, is_dir) in entries {
            if is_dir {
                stack.push(path);
            } else if files.len() < limit {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::FileTree;
    use penumbra::{Buffer as Surface, Theme};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Builds a throwaway project tree: `README.md`, `zebra.txt`, and `src/main.rs`.
    fn temp_tree() -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("majestic-tree-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        fs::write(dir.join("zebra.txt"), "").unwrap();
        fs::write(dir.join("src").join("main.rs"), "").unwrap();
        dir
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.rows
            .iter()
            .map(|row| super::file_name(&row.path))
            .collect()
    }

    #[test]
    fn lists_root_children_dirs_first_then_sorted() {
        let dir = temp_tree();
        let tree = FileTree::new(&dir);
        // `src` (directory) sorts before the files; files are alphabetical.
        assert_eq!(names(&tree), vec!["src", "README.md", "zebra.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untracked_files_render_in_the_git_added_color() {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("majestic-tree-git-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .output();
        if !init.is_ok_and(|out| out.status.success()) {
            eprintln!("skipping: git unavailable");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        // Two untracked files: row 0 is selected (takes the selection highlight), so check the
        // non-selected one carries the git "added/untracked" foreground (Radium Green).
        fs::write(dir.join("alpha.txt"), "x").unwrap();
        fs::write(dir.join("beta.txt"), "y").unwrap();

        let theme = Theme::steelbore();
        let mut tree = FileTree::new(&dir);
        let mut surface = Surface::new(30, 6, theme.base_style());
        let area = surface.area();
        tree.render(&mut surface, area, &theme, false);

        let has_green = (1..surface.height()).any(|y| {
            (0..surface.width()).any(|x| {
                surface
                    .cell(x, y)
                    .is_some_and(|cell| cell.symbol != ' ' && cell.style.fg == theme.success)
            })
        });
        assert!(
            has_green,
            "an untracked file should render in the success color"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expanding_a_directory_reveals_its_children() {
        let dir = temp_tree();
        let mut tree = FileTree::new(&dir);
        // `src` is selected first (row 0); activating it expands the directory.
        assert!(tree.activate().is_none());
        assert_eq!(
            names(&tree),
            vec!["src", "main.rs", "README.md", "zebra.txt"]
        );
        // The revealed child is nested one level deeper.
        assert_eq!(tree.rows[1].depth, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn activating_a_file_returns_its_path() {
        let dir = temp_tree();
        let mut tree = FileTree::new(&dir);
        tree.select_down(); // README.md
        let opened = tree.activate().unwrap();
        assert_eq!(opened.file_name().unwrap(), "README.md");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renders_title_and_entries() {
        let dir = temp_tree();
        let mut tree = FileTree::new(&dir);
        let theme = Theme::steelbore();
        let mut surface = Surface::new(24, 6, theme.base_style());
        let area = surface.area();
        tree.render(&mut surface, area, &theme, true);

        let row_text = |y: u16| -> String {
            (0..surface.width())
                .filter_map(|x| surface.cell(x, y).map(|c| c.symbol))
                .collect()
        };
        assert!(
            row_text(0).contains("EXPLORER"),
            "title row: {:?}",
            row_text(0)
        );
        let body: String = (1..surface.height()).map(row_text).collect();
        assert!(body.contains("src"), "tree should list `src`: {body:?}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
