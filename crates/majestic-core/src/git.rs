// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Git working-tree status for the explorer (UI.md §2 git indicators).
//!
//! A thin wrapper over `git status --porcelain` — no `git2`/`gix` dependency, no network, purely
//! local (PFA, §7). Failure to find `git` or a repository is not an error: [`statuses`] simply
//! returns an empty map and the explorer shows no decorations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A file's working-tree status, condensed to what the explorer colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitStatus {
    /// Tracked and changed (worktree or index), or renamed/copied.
    Modified,
    /// Newly added and staged.
    Added,
    /// Not tracked by git.
    Untracked,
    /// Removed from the worktree.
    Deleted,
}

/// Maps each changed path under `root`'s repository to its [`GitStatus`] (absolute paths).
///
/// Clean files are absent from the map. Returns empty when `root` is not a git work tree or
/// `git` is unavailable — never panics or surfaces an error to the editor.
#[must_use]
pub fn statuses(root: &Path) -> HashMap<PathBuf, GitStatus> {
    let mut map = HashMap::new();
    // `-c core.quotePath=false` keeps non-ASCII paths literal; `-C root` reports paths relative
    // to `root`, which we re-absolutize to match the explorer's entries.
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false", "status", "--porcelain"])
        .output()
    else {
        return map; // git not installed
    };
    if !output.status.success() {
        return map; // not a repository, etc.
    }

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // Each line is `XY<space>path`; renames render as `XY old -> new`.
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let mut path = &line[3..];
        if let Some((_, renamed_to)) = path.split_once(" -> ") {
            path = renamed_to;
        }
        map.insert(root.join(path), classify(code));
    }
    map
}

/// Condenses a two-character porcelain status code into a [`GitStatus`].
fn classify(code: &str) -> GitStatus {
    if code == "??" {
        GitStatus::Untracked
    } else if code.contains('D') {
        GitStatus::Deleted
    } else if code.contains('A') {
        GitStatus::Added
    } else {
        GitStatus::Modified // M, R, C, T, …
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, statuses, GitStatus};
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn classify_porcelain_codes() {
        assert_eq!(classify("??"), GitStatus::Untracked);
        assert_eq!(classify(" M"), GitStatus::Modified);
        assert_eq!(classify("MM"), GitStatus::Modified);
        assert_eq!(classify("A "), GitStatus::Added);
        assert_eq!(classify(" D"), GitStatus::Deleted);
        assert_eq!(classify("R "), GitStatus::Modified);
    }

    #[test]
    fn non_repository_yields_empty() {
        let dir = std::env::temp_dir().join(format!("majestic-git-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(statuses(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_file_is_reported() {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("majestic-git-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let init = Command::new("git").arg("-C").arg(&dir).arg("init").output();
        if !init.is_ok_and(|out| out.status.success()) {
            eprintln!("skipping: git unavailable");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        fs::write(dir.join("new.txt"), "hi").unwrap();

        let map = statuses(&dir);
        assert_eq!(
            map.get(&dir.join("new.txt")).copied(),
            Some(GitStatus::Untracked)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
