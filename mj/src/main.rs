// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic — the `mj` binary entry point.
//!
//! TUI-first terminal, editor, and coding agent — Concept #1 (Rust + Steel).
//! See the workspace `MAJESTIC.md` for architecture and the milestone roadmap.
//!
//! Scaffold (M0): this binary establishes the `--version` / `--help` surfaces
//! (GNU + Spacecraft §13.2 attribution) and recognizes the planned command set.
//! The editor engine, the subcommands, and full SFRS `--json` output land in the
//! later M0–M4 steps; until then non-help invocations report "not yet implemented"
//! and exit non-zero rather than pretending to work.

use std::process::ExitCode;

use majestic_core::{Buffer, Editor, Workspace};

mod tui;

/// Canonical program name (GNU `--version` discipline: a constant, never `argv[0]`).
const PROGRAM: &str = "mj";

/// Project version, sourced from the crate manifest.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the parsed command line asks the program to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Print version + attribution and exit successfully.
    Version,
    /// Print usage help and exit successfully.
    Help,
    /// Open the given paths in the editor (not yet implemented at scaffold stage).
    Open(Vec<String>),
    /// A recognized subcommand that is not yet implemented.
    Pending(String),
    /// No arguments: would open an empty editor (not yet implemented).
    Empty,
    /// An unrecognized option.
    Unknown(String),
}

/// Classify raw command-line arguments (everything after the program name).
///
/// Kept pure and total so it can be unit-tested without running the program.
fn classify(args: &[String]) -> Action {
    let Some(first) = args.first() else {
        return Action::Empty;
    };
    match first.as_str() {
        "--version" | "-V" => Action::Version,
        "--help" | "-h" => Action::Help,
        // Recognized noun-verb subcommands (SFRS); implemented in later milestones.
        "config" | "session" | "describe" | "ed" => Action::Pending(first.clone()),
        // `--` terminates option parsing; everything after is a file path.
        "--" => Action::Open(args[1..].to_vec()),
        other if other.starts_with('-') => Action::Unknown(other.to_owned()),
        _ => Action::Open(args.to_vec()),
    }
}

/// Print the `--version` block, including the Spacecraft §13.2 attribution.
fn print_version() {
    println!(
        "\
{PROGRAM} {VERSION}
Majestic — TUI-first terminal, editor, and coding agent.

Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
Copyright (C) 2026 Mohamed Hammad & Spacecraft Software  |  License: GPL-3.0-or-later
https://Majestic.SpacecraftSoftware.org/"
    );
}

/// Print the `--help` text, ending with the §13.2 maintainer/URL footer.
fn print_help() {
    println!(
        "\
{PROGRAM} {VERSION} — Majestic

USAGE:
    {PROGRAM} [FILE...]            Open files in the editor
    {PROGRAM} <COMMAND> [ARGS]     Run a subcommand

COMMANDS:
    config     Validate/inspect configuration (M1)
    session    Manage daemon sessions (M2)
    describe   Oracle introspection from the shell (M1)
    ed         Line-editor mode (M4)

OPTIONS:
    -h, --help       Print this help
    -V, --version    Print version and attribution

Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
https://Majestic.SpacecraftSoftware.org/"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify(&args) {
        Action::Version => {
            print_version();
            ExitCode::SUCCESS
        }
        Action::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Action::Empty => run_editor(&[]),
        Action::Open(paths) => run_editor(&paths),
        Action::Pending(cmd) => {
            eprintln!("{PROGRAM}: subcommand `{cmd}` is not yet implemented (later milestone).");
            ExitCode::FAILURE
        }
        Action::Unknown(opt) => {
            eprintln!("{PROGRAM}: unknown option `{opt}`. Try `{PROGRAM} --help`.");
            ExitCode::FAILURE
        }
    }
}

/// Opens every `path` as a buffer (a scratch buffer when none) and runs the interactive editor.
///
/// Each file becomes a tab; the first is shown in the sole pane and the rest are background
/// tabs (`Alt+←/→` to switch, `Ctrl+\` to split). Any path that fails to open aborts startup.
fn run_editor(paths: &[String]) -> ExitCode {
    let mut editors = Vec::with_capacity(paths.len());
    for path in paths {
        match Buffer::open(path) {
            Ok(buffer) => editors.push(Editor::with_buffer(buffer)),
            Err(error) => {
                eprintln!("{PROGRAM}: cannot open {path}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if editors.is_empty() {
        editors.push(Editor::new());
    }
    match tui::run(Workspace::from_editors(editors)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{PROGRAM}: terminal error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, Action};

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flags_classify_to_version_and_help() {
        assert_eq!(classify(&owned(&["--version"])), Action::Version);
        assert_eq!(classify(&owned(&["-V"])), Action::Version);
        assert_eq!(classify(&owned(&["--help"])), Action::Help);
        assert_eq!(classify(&owned(&["-h"])), Action::Help);
    }

    #[test]
    fn no_args_is_empty() {
        assert_eq!(classify(&[]), Action::Empty);
    }

    #[test]
    fn files_subcommands_and_terminator() {
        assert_eq!(
            classify(&owned(&["a.txt", "b.rs"])),
            Action::Open(owned(&["a.txt", "b.rs"]))
        );
        assert_eq!(
            classify(&owned(&["config", "check"])),
            Action::Pending("config".to_owned())
        );
        assert_eq!(
            classify(&owned(&["--", "-weird.txt"])),
            Action::Open(owned(&["-weird.txt"]))
        );
        assert_eq!(
            classify(&owned(&["--nope"])),
            Action::Unknown("--nope".to_owned())
        );
    }
}
