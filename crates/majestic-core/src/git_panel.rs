// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The **Git status panel** (M4) — a Magit-role porcelain inside Majestic: a modal view of the working
//! tree (branch, staged / unstaged / untracked changes) with stage, unstage, commit, diff, and refresh
//! actions, all driven from the keyboard.
//!
//! Like the explorer's [`git`](crate::git) decorations it is a thin wrapper over the `git` CLI — no
//! `git2`/`gix` dependency, purely local (Standard §7). It parses `git status --porcelain -b` into
//! [`RepoStatus`] and runs `git add` / `git reset` / `git commit` / `git diff` for the actions. Opening
//! in a non-repository fails cleanly (the panel does not open).
//
// Rust guideline compliant 2026-05-18

use std::path::{Path, PathBuf};
use std::process::Command;

use penumbra::{Buffer, Rect, Style, Theme};

/// Which part of the working tree a change belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Staged,
    Unstaged,
    Untracked,
}

/// One changed file, with its porcelain status code (`M`, `A`, `D`, `R`, `?`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    code: char,
    path: String,
}

/// The parsed working-tree status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RepoStatus {
    branch: String,
    ahead: u32,
    behind: u32,
    staged: Vec<Change>,
    unstaged: Vec<Change>,
    untracked: Vec<Change>,
}

/// One row in the flattened display: a section header or a file under it.
#[derive(Debug)]
enum Row {
    Header(Section, usize),
    File(Section, usize),
}

/// A scrolling diff sub-view (`d`), shown over the status list until dismissed.
#[derive(Debug)]
struct DiffView {
    title: String,
    lines: Vec<String>,
    scroll: usize,
}

/// The Magit-role Git status panel (M4): construct with [`GitPanel::open`], render each frame, drive
/// with the keyboard. Holds the repo root, the latest [`RepoStatus`], the selection, and a transient
/// notice / diff sub-view.
#[derive(Debug)]
pub struct GitPanel {
    root: PathBuf,
    status: RepoStatus,
    rows: Vec<Row>,
    /// Index into `rows`; always points at a `Row::File` when one exists.
    selected: usize,
    scroll: usize,
    notice: Option<String>,
    diff: Option<DiffView>,
}

impl GitPanel {
    /// Opens the panel on the repository containing `root`, or returns `None` if `root` is not a git
    /// work tree (or `git` is unavailable).
    #[must_use]
    pub fn open(root: &Path) -> Option<Self> {
        let porcelain = run_git(
            root,
            &["-c", "core.quotePath=false", "status", "--porcelain", "-b"],
        )
        .ok()?;
        let status = parse_status(&porcelain);
        let mut panel = Self {
            root: root.to_path_buf(),
            status,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            notice: None,
            diff: None,
        };
        panel.rebuild_rows();
        Some(panel)
    }

    /// Re-reads `git status`, preserving the selection position where possible.
    pub fn refresh(&mut self) {
        if let Ok(porcelain) = run_git(
            &self.root,
            &["-c", "core.quotePath=false", "status", "--porcelain", "-b"],
        ) {
            self.status = parse_status(&porcelain);
        }
        self.rebuild_rows();
    }

    /// Rebuilds the flattened `rows` from the current status and clamps the selection to a file row.
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for (section, changes) in [
            (Section::Staged, &self.status.staged),
            (Section::Unstaged, &self.status.unstaged),
            (Section::Untracked, &self.status.untracked),
        ] {
            if changes.is_empty() {
                continue;
            }
            rows.push(Row::Header(section, changes.len()));
            for index in 0..changes.len() {
                rows.push(Row::File(section, index));
            }
        }
        self.rows = rows;
        if self.selected >= self.rows.len()
            || !matches!(self.rows.get(self.selected), Some(Row::File(..)))
        {
            self.selected = self.first_file().unwrap_or(0);
        }
    }

    /// The row index of the first file, if any.
    fn first_file(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::File(..)))
    }

    /// Moves the selection to the next (`forward`) or previous file row, skipping headers.
    pub fn select(&mut self, forward: bool) {
        if self.rows.is_empty() {
            return;
        }
        let mut index = self.selected;
        loop {
            index = if forward {
                if index + 1 >= self.rows.len() {
                    return;
                }
                index + 1
            } else {
                if index == 0 {
                    return;
                }
                index - 1
            };
            if matches!(self.rows.get(index), Some(Row::File(..))) {
                self.selected = index;
                return;
            }
        }
    }

    /// The `(section, change)` currently selected, if any.
    fn selected_change(&self) -> Option<(Section, &Change)> {
        match self.rows.get(self.selected)? {
            Row::File(section, index) => {
                let change = match section {
                    Section::Staged => self.status.staged.get(*index),
                    Section::Unstaged => self.status.unstaged.get(*index),
                    Section::Untracked => self.status.untracked.get(*index),
                }?;
                Some((*section, change))
            }
            Row::Header(..) => None,
        }
    }

    /// `s` — stages the selected file (`git add`).
    pub fn stage(&mut self) {
        let Some((_, change)) = self.selected_change() else {
            return;
        };
        let path = change.path.clone();
        self.run_action(&["add", "--", &path], &format!("staged {path}"));
    }

    /// `S` — stages every change (`git add -A`).
    pub fn stage_all(&mut self) {
        self.run_action(&["add", "-A"], "staged all changes");
    }

    /// `u` — unstages the selected file (`git reset -q HEAD`).
    pub fn unstage(&mut self) {
        let Some((_, change)) = self.selected_change() else {
            return;
        };
        let path = change.path.clone();
        self.run_action(
            &["reset", "-q", "HEAD", "--", &path],
            &format!("unstaged {path}"),
        );
    }

    /// `U` — unstages everything (`git reset -q HEAD`).
    pub fn unstage_all(&mut self) {
        self.run_action(&["reset", "-q", "HEAD"], "unstaged all changes");
    }

    /// `c` — commits the staged changes with `message`.
    pub fn commit(&mut self, message: &str) {
        if message.trim().is_empty() {
            self.notice = Some("commit aborted: empty message".to_owned());
            return;
        }
        if self.status.staged.is_empty() {
            self.notice = Some("nothing staged to commit".to_owned());
            return;
        }
        self.run_action(&["commit", "-m", message], "committed");
    }

    /// Runs a git subcommand, records a notice (the failure's stderr on error), and refreshes.
    fn run_action(&mut self, args: &[&str], success: &str) {
        match run_git(&self.root, args) {
            Ok(_) => self.notice = Some(success.to_owned()),
            Err(error) => self.notice = Some(one_line(&error)),
        }
        self.refresh();
    }

    /// `d` — opens a scrolling diff of the selected file (staged or worktree, by section).
    pub fn show_diff(&mut self) {
        let Some((section, change)) = self.selected_change() else {
            return;
        };
        let path = change.path.clone();
        let (args, label): (Vec<&str>, &str) = match section {
            Section::Staged => (vec!["diff", "--staged", "--", &path], "staged"),
            Section::Unstaged => (vec!["diff", "--", &path], "worktree"),
            Section::Untracked => {
                self.notice = Some("untracked file — stage it to diff".to_owned());
                return;
            }
        };
        match run_git(&self.root, &args) {
            Ok(text) if !text.trim().is_empty() => {
                self.diff = Some(DiffView {
                    title: format!("{path} ({label})"),
                    lines: text.lines().map(str::to_owned).collect(),
                    scroll: 0,
                });
            }
            Ok(_) => self.notice = Some(format!("no {label} changes in {path}")),
            Err(error) => self.notice = Some(one_line(&error)),
        }
    }

    /// Whether the diff sub-view is open (its keys are scroll + close).
    #[must_use]
    pub fn diff_open(&self) -> bool {
        self.diff.is_some()
    }

    /// Closes the diff sub-view (back to the status list).
    pub fn close_diff(&mut self) {
        self.diff = None;
    }

    /// Scrolls the diff sub-view (positive `delta` down).
    pub fn scroll_diff(&mut self, delta: i32) {
        if let Some(diff) = self.diff.as_mut() {
            let magnitude = usize::try_from(delta.unsigned_abs()).unwrap_or(0);
            diff.scroll = if delta < 0 {
                diff.scroll.saturating_sub(magnitude)
            } else {
                diff.scroll.saturating_add(magnitude)
            };
        }
    }

    /// Renders the panel (header + status list, or the diff sub-view) into `area`.
    pub fn render(&mut self, surface: &mut Buffer, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        if self.diff.is_some() {
            self.render_diff(surface, area, theme);
            return;
        }
        self.render_status(surface, area, theme);
    }

    /// Renders the status list (the default view).
    fn render_status(&mut self, surface: &mut Buffer, area: Rect, theme: &Theme) {
        let (header, body) = area.split_top(1);
        let header_style = Style::new(theme.background, theme.accent);
        let title = format!(
            " Git · {} · s stage · u unstage · c commit · d diff · g refresh · q close",
            self.branch_label()
        );
        fill_row(surface, header, header_style);
        surface.set_str(header.x, header.y, &title, header_style);
        if body.is_empty() {
            return;
        }

        // Keep the selection on screen.
        let height = usize::from(body.height);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }

        let blank = Style::new(theme.foreground, theme.background);
        for row in 0..body.height {
            let y = body.y + row;
            fill_row(surface, Rect::new(body.x, y, body.width, 1), blank);
            let Some(item) = self.rows.get(self.scroll + usize::from(row)) else {
                if self.rows.is_empty() && row == 0 {
                    surface.set_str(body.x, y, "  working tree clean", blank);
                }
                continue;
            };
            let selected = self.scroll + usize::from(row) == self.selected;
            draw_row(
                surface,
                Rect::new(body.x, y, body.width, 1),
                item,
                &self.status,
                theme,
                selected,
            );
        }

        if let Some(notice) = &self.notice {
            let style = Style::new(theme.info, theme.background);
            let last = body.y + body.height - 1;
            fill_row(surface, Rect::new(body.x, last, body.width, 1), blank);
            surface.set_str(body.x, last, &format!("  {notice}"), style);
        }
    }

    /// Renders the diff sub-view.
    fn render_diff(&mut self, surface: &mut Buffer, area: Rect, theme: &Theme) {
        let (header, body) = area.split_top(1);
        let header_style = Style::new(theme.background, theme.accent);
        fill_row(surface, header, header_style);
        let title = self
            .diff
            .as_ref()
            .map_or_else(String::new, |d| format!(" Diff · {} · q back", d.title));
        surface.set_str(header.x, header.y, &title, header_style);
        let Some(diff) = self.diff.as_mut() else {
            return;
        };
        let height = usize::from(body.height);
        let max_scroll = diff.lines.len().saturating_sub(height);
        diff.scroll = diff.scroll.min(max_scroll);
        let blank = Style::new(theme.foreground, theme.background);
        for row in 0..body.height {
            let y = body.y + row;
            fill_row(surface, Rect::new(body.x, y, body.width, 1), blank);
            let Some(line) = diff.lines.get(diff.scroll + usize::from(row)) else {
                continue;
            };
            let style = diff_line_style(line, theme);
            surface.set_str(body.x, y, line, style);
        }
    }

    /// The branch summary, e.g. `main ↑1 ↓2`.
    fn branch_label(&self) -> String {
        let ahead = if self.status.ahead > 0 {
            format!(" ↑{}", self.status.ahead)
        } else {
            String::new()
        };
        let behind = if self.status.behind > 0 {
            format!(" ↓{}", self.status.behind)
        } else {
            String::new()
        };
        format!("{}{ahead}{behind}", self.status.branch)
    }
}

/// Fills `rect`'s single row with spaces in `style`.
fn fill_row(surface: &mut Buffer, rect: Rect, style: Style) {
    for x in rect.x..rect.right() {
        surface.set_char(x, rect.y, ' ', style);
    }
}

/// Draws one status row (a section header or a file entry) into single-row `rect`.
fn draw_row(
    surface: &mut Buffer,
    rect: Rect,
    row: &Row,
    status: &RepoStatus,
    theme: &Theme,
    selected: bool,
) {
    let background = if selected {
        theme.accent
    } else {
        theme.background
    };
    match row {
        Row::Header(section, count) => {
            // Headers are never the selection target (it lands on file rows), so always plain.
            let style = Style::new(theme.accent, theme.background).bold();
            surface.set_str(
                rect.x,
                rect.y,
                &format!("{} ({count})", section_title(*section)),
                style,
            );
        }
        Row::File(section, index) => {
            let change = match section {
                Section::Staged => status.staged.get(*index),
                Section::Unstaged => status.unstaged.get(*index),
                Section::Untracked => status.untracked.get(*index),
            };
            let Some(change) = change else { return };
            let foreground = if selected {
                theme.background
            } else {
                code_color(change.code, theme)
            };
            let style = Style::new(foreground, background);
            let label = format!("  {:<9} {}", code_label(change.code), change.path);
            let mut col = surface.set_str(rect.x, rect.y, &label, style);
            // Extend the highlight to the row's end when selected.
            if selected {
                while col < rect.right() {
                    surface.set_char(col, rect.y, ' ', style);
                    col += 1;
                }
            }
        }
    }
}

/// A human title for a section.
fn section_title(section: Section) -> &'static str {
    match section {
        Section::Staged => "Staged changes",
        Section::Unstaged => "Unstaged changes",
        Section::Untracked => "Untracked files",
    }
}

/// A word for a porcelain status code.
fn code_label(code: char) -> &'static str {
    match code {
        'A' => "new file",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "typechange",
        '?' => "untracked",
        _ => "modified",
    }
}

/// The Steelbore color for a status code.
fn code_color(code: char, theme: &Theme) -> penumbra::Rgb {
    match code {
        'A' => theme.success,
        'D' => theme.error,
        '?' => theme.info,
        _ => theme.foreground,
    }
}

/// The color for a diff line (`+` added, `-` removed, `@@` hunk header).
fn diff_line_style(line: &str, theme: &Theme) -> Style {
    let foreground = if line.starts_with('+') {
        theme.success
    } else if line.starts_with('-') {
        theme.error
    } else if line.starts_with("@@") {
        theme.accent
    } else {
        theme.foreground
    };
    Style::new(foreground, theme.background)
}

/// Runs `git -C root <args>`, returning stdout on success or stderr as the error.
fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("git unavailable: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Flattens a multi-line message into one line.
fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses `git status --porcelain -b` output into a [`RepoStatus`].
fn parse_status(porcelain: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    for line in porcelain.lines() {
        if let Some(branch_line) = line.strip_prefix("## ") {
            parse_branch(branch_line, &mut status);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let mut codes = line.chars();
        let index = codes.next().unwrap_or(' ');
        let worktree = codes.next().unwrap_or(' ');
        let mut path = &line[3..];
        if let Some((_, renamed_to)) = path.split_once(" -> ") {
            path = renamed_to;
        }
        if index == '?' && worktree == '?' {
            status.untracked.push(Change {
                code: '?',
                path: path.to_owned(),
            });
            continue;
        }
        if index != ' ' {
            status.staged.push(Change {
                code: index,
                path: path.to_owned(),
            });
        }
        if worktree != ' ' {
            status.unstaged.push(Change {
                code: worktree,
                path: path.to_owned(),
            });
        }
    }
    status
}

/// Parses the `## branch...upstream [ahead N, behind M]` header line (also `## No commits yet on
/// branch` for a fresh repository, and `## HEAD (no branch)` when detached).
fn parse_branch(line: &str, status: &mut RepoStatus) {
    let line = line.strip_prefix("No commits yet on ").unwrap_or(line);
    let head = line.split([' ', '.']).next().unwrap_or(line);
    status.branch = if head.is_empty() {
        "(detached)".to_owned()
    } else {
        head.to_owned()
    };
    if let Some(bracket) = line
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
    {
        for part in bracket.0.split(',') {
            let part = part.trim();
            if let Some(count) = part.strip_prefix("ahead ") {
                status.ahead = count.trim().parse().unwrap_or(0);
            } else if let Some(count) = part.strip_prefix("behind ") {
                status.behind = count.trim().parse().unwrap_or(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_status, Section};

    #[test]
    fn parses_branch_with_ahead_behind() {
        let status = parse_status("## main...origin/main [ahead 2, behind 1]\n");
        assert_eq!(status.branch, "main");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn classifies_staged_unstaged_untracked() {
        // Built with explicit "\n" (not line-continuation, which would strip the porcelain format's
        // significant leading spaces — e.g. the " M" worktree-only status).
        let porcelain =
            "## main\nM  staged.rs\n M unstaged.rs\nMM both.rs\nA  added.rs\n?? new.txt\n";
        let status = parse_status(porcelain);
        assert_eq!(status.staged.len(), 3, "M , MM, A  are staged"); // staged.rs, both.rs, added.rs
        assert_eq!(status.unstaged.len(), 2, " M and MM are unstaged"); // unstaged.rs, both.rs
        assert_eq!(status.untracked.len(), 1);
        assert_eq!(status.untracked[0].path, "new.txt");
        assert_eq!(status.staged[2].code, 'A');
    }

    #[test]
    fn a_rename_takes_the_new_path() {
        let status = parse_status("## main\nR  old.rs -> new.rs\n");
        assert_eq!(status.staged.len(), 1);
        assert_eq!(status.staged[0].path, "new.rs");
        assert_eq!(status.staged[0].code, 'R');
    }

    #[test]
    fn section_titles_are_human() {
        assert_eq!(super::section_title(Section::Staged), "Staged changes");
        assert_eq!(super::section_title(Section::Untracked), "Untracked files");
    }

    #[test]
    fn stage_then_commit_against_a_real_repo() {
        use super::GitPanel;
        use std::fs;
        use std::process::Command;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("majestic-gitpanel-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output();
        if !git(&["init"]).is_ok_and(|out| out.status.success()) {
            eprintln!("skipping: git unavailable");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        // A self-contained identity, no signing — the test must not depend on the user's git config.
        let _ = git(&["config", "user.email", "test@example.com"]);
        let _ = git(&["config", "user.name", "Test"]);
        let _ = git(&["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("file.txt"), "one\n").unwrap();
        let _ = git(&["add", "-A"]);
        let _ = git(&["commit", "-m", "base"]);

        // Modify the file → it shows as an unstaged change.
        fs::write(dir.join("file.txt"), "two\n").unwrap();
        let mut panel = GitPanel::open(&dir).expect("a git work tree");
        assert_eq!(panel.status.unstaged.len(), 1, "the edit is unstaged");
        assert!(panel.status.staged.is_empty());

        // Stage it (the selection is the first file) → it moves to the staged section.
        panel.stage();
        assert_eq!(panel.status.staged.len(), 1, "stage moved it to the index");
        assert!(panel.status.unstaged.is_empty());

        // Commit → the working tree is clean again.
        panel.commit("second");
        assert!(
            panel.status.staged.is_empty() && panel.status.unstaged.is_empty(),
            "clean after commit: {:?}",
            panel.status
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
