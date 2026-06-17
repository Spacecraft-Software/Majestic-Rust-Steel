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

use majestic_config::Config;
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
        --safe       Skip the user configuration (Nickel manifest)

Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
https://Majestic.SpacecraftSoftware.org/"
    );
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // `--safe` is a global flag: skip the user configuration (Standard §5 safe mode). It is
    // stripped before noun-verb classification so it can appear anywhere on the line.
    let safe_mode = raw.iter().any(|arg| arg == "--safe");
    let args: Vec<String> = raw.into_iter().filter(|arg| arg != "--safe").collect();
    match classify(&args) {
        Action::Version => {
            print_version();
            ExitCode::SUCCESS
        }
        Action::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Action::Empty => run_editor(&[], safe_mode),
        Action::Open(paths) => run_editor(&paths, safe_mode),
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
/// Unless `safe_mode` is set, the Nickel manifest is loaded and applied before launch.
fn run_editor(paths: &[String], safe_mode: bool) -> ExitCode {
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
    let mut workspace = Workspace::from_editors(editors);
    if !safe_mode {
        apply_config(&mut workspace);
    }
    match tui::run(workspace) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{PROGRAM}: terminal error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Loads the discovered Nickel manifest and applies it to `workspace`.
///
/// Fail-soft: a missing manifest is normal (defaults stand); a malformed one keeps the defaults
/// and surfaces a short notice in the status bar (run with `--safe` to skip the manifest).
fn apply_config(workspace: &mut Workspace) {
    let Some(path) = Config::discover() else {
        return; // no manifest -> built-in defaults
    };
    match Config::load(&path) {
        Ok(config) => workspace.set_tab_width(config.tab_width()),
        Err(error) => {
            // Flatten the (multi-line) Nickel diagnostic into one status-bar line.
            let detail = error
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            workspace.set_status(format!(
                "config {} invalid — using defaults; --safe to skip. {detail}",
                path.display(),
            ));
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
