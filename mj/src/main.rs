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

use keymaker::Profile;
use majestic_config::Config;
use majestic_core::{Buffer, Editor, Session, Workspace};
use majestic_steel::Runtime as SteelRuntime;

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
    /// Oracle: describe a command/variable, or (with no argument) list keybindings.
    Describe(Option<String>),
    /// Oracle: search commands by keyword.
    Apropos(Option<String>),
    /// Open the built-in Info reader: a topic's manual, or the `dir` directory with no argument.
    Info(Option<String>),
    /// `session [list|clear]`: manage the saved session (WS2).
    Session(Option<String>),
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
        // Oracle introspection from the shell (PRD §5.2.2).
        "describe" => Action::Describe(args.get(1).cloned()),
        "apropos" => Action::Apropos(args.get(1).cloned()),
        "info" => Action::Info(args.get(1).cloned()),
        "session" => Action::Session(args.get(1).cloned()),
        // Recognized noun-verb subcommands (SFRS); implemented in later milestones.
        "config" | "ed" => Action::Pending(first.clone()),
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
    describe [NAME]    Help for a command/variable, or list keybindings (Oracle)
    apropos <WORD>     Search commands by keyword (Oracle)
    info [TOPIC]       Open the built-in Info/Texinfo reader (the `dir` index if no topic)
    config             Validate/inspect configuration (M1)
    session [list|clear]  Show or clear the saved session (`mj` reopens it)
    ed                 Line-editor mode (M4)

OPTIONS:
    -h, --help       Print this help
    -V, --version    Print version and attribution
        --safe       Skip the user configuration (Nickel manifest and config.scm)

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
        Action::Describe(query) => run_describe(query.as_deref()),
        Action::Apropos(query) => run_apropos(query.as_deref()),
        Action::Info(topic) => run_info(topic.as_deref(), safe_mode),
        Action::Session(sub) => run_session(sub.as_deref()),
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
    let mut initial_info: Option<std::path::PathBuf> = None;
    for path in paths {
        // The first `.info` argument opens in the built-in Info reader, not the text editor.
        if initial_info.is_none()
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e == "info")
        {
            initial_info = Some(path.into());
            continue;
        }
        match Buffer::open(path) {
            Ok(buffer) => editors.push(Editor::with_buffer(buffer)),
            Err(error) => {
                eprintln!("{PROGRAM}: cannot open {path}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    // Plain `mj` (no file arguments) reopens the last saved session, if any; otherwise a scratch
    // buffer. Launching with files opens those instead and does not restore.
    let mut workspace = if paths.is_empty() {
        Session::load().map_or_else(
            || Workspace::from_editors(vec![Editor::new()]),
            |session| Workspace::from_session(&session),
        )
    } else {
        if editors.is_empty() {
            editors.push(Editor::new());
        }
        Workspace::from_editors(editors)
    };
    // First run = no manifest yet (and not in safe mode): prompt for a keybinding profile.
    let first_run = !safe_mode && Config::discover().is_none();
    if !safe_mode {
        apply_config(&mut workspace);
    }
    // The editor path persists its layout on exit so the next plain `mj` resumes here.
    match tui::run(workspace, initial_info, first_run, true) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{PROGRAM}: terminal error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Manages the saved session: `session` / `session list` prints it, `session clear` deletes it.
fn run_session(sub: Option<&str>) -> ExitCode {
    match sub {
        None | Some("list") => {
            let Some(session) = Session::load() else {
                println!("No saved session.");
                return ExitCode::SUCCESS;
            };
            println!(
                "Saved session — {} pane(s), focused #{}:",
                session.panes.len(),
                session.focused
            );
            for (index, pane) in session.panes.iter().enumerate() {
                let location = pane
                    .path
                    .as_deref()
                    .map_or_else(|| "[scratch]".to_owned(), |path| path.display().to_string());
                println!("  {index}: {location}");
            }
            ExitCode::SUCCESS
        }
        Some("clear") => {
            let Some(path) = Session::default_path() else {
                eprintln!("{PROGRAM}: no state directory to clear");
                return ExitCode::FAILURE;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!("Cleared session {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    println!("No saved session.");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{PROGRAM}: cannot clear session: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("{PROGRAM}: unknown `session` subcommand `{other}` (try: list, clear)");
            ExitCode::FAILURE
        }
    }
}

/// Opens the built-in Info reader on `topic`'s manual (`<topic>.info` from the Info search path),
/// or the `dir` directory when no topic is given.
fn run_info(topic: Option<&str>, safe_mode: bool) -> ExitCode {
    let Some(path) = resolve_info(topic) else {
        match topic {
            Some(name) => eprintln!("{PROGRAM}: no Info manual found for `{name}`"),
            None => eprintln!("{PROGRAM}: no Info directory (`dir`) on the Info search path"),
        }
        return ExitCode::FAILURE;
    };
    let mut workspace = Workspace::from_editors(vec![Editor::new()]);
    if !safe_mode {
        apply_config(&mut workspace);
    }
    // `mj info` is a transient manual view: no first-run prompt, and it must not overwrite the
    // saved editing session.
    match tui::run(workspace, Some(path), false, false) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{PROGRAM}: terminal error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves `topic` to an Info file path (`<topic>.info`, then `<topic>`), or the `dir` file when
/// `topic` is `None`, searching `$INFOPATH` then standard system directories.
fn resolve_info(topic: Option<&str>) -> Option<std::path::PathBuf> {
    let names: Vec<String> = match topic {
        Some(name) => vec![format!("{name}.info"), name.to_owned()],
        None => vec!["dir".to_owned()],
    };
    info_dirs().into_iter().find_map(|dir| {
        names
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    })
}

/// The Info search path: `$INFOPATH` (colon-separated) then standard system directories.
fn info_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut dirs: Vec<PathBuf> = std::env::var("INFOPATH")
        .into_iter()
        .flat_map(|path| {
            path.split(':')
                .filter(|part| !part.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect();
    for standard in [
        "/run/current-system/sw/share/info",
        "/usr/share/info",
        "/usr/local/share/info",
    ] {
        dirs.push(PathBuf::from(standard));
    }
    dirs
}

/// Loads the hybrid configuration and applies it to `workspace`.
///
/// The declarative Nickel manifest sets the base; the imperative Steel `config.scm` then layers
/// overrides on top (last writer wins). Fail-soft: missing files are normal (defaults stand); a
/// malformed manifest or script keeps the working settings and surfaces a one-line status notice
/// (run with `--safe` to skip configuration entirely).
fn apply_config(workspace: &mut Workspace) {
    let mut tab_width: Option<usize> = None;
    let mut keymap_name: Option<String> = None;
    let mut notices: Vec<String> = Vec::new();

    // 1. Declarative half — the Nickel manifest.
    if let Some(path) = Config::discover() {
        match Config::load(&path) {
            Ok(config) => {
                tab_width = Some(config.tab_width());
                keymap_name = Some(config.keymap.clone());
            }
            Err(error) => notices.push(format!(
                "manifest {} invalid ({})",
                path.display(),
                one_line(&error)
            )),
        }
    }

    // 2. Imperative half — the Steel config.scm, layered on top.
    if let Some(path) = SteelRuntime::discover() {
        let mut runtime = SteelRuntime::new();
        match runtime.run_file(&path) {
            Ok(()) => {
                if let Some(columns) = runtime.settings().tab_width {
                    tab_width = Some(columns.clamp(1, 16));
                }
                if let Some(name) = runtime.settings().keymap.clone() {
                    keymap_name = Some(name);
                }
            }
            Err(error) => notices.push(format!(
                "config.scm {} failed ({})",
                path.display(),
                one_line(&error)
            )),
        }
    }

    if let Some(columns) = tab_width {
        workspace.set_tab_width(columns);
    }
    // An unknown profile name keeps the default (fail-soft) and surfaces a notice.
    if let Some(name) = keymap_name {
        match Profile::from_name(&name) {
            Some(profile) => workspace.set_profile(profile),
            None => notices.push(format!("unknown keymap profile `{name}`")),
        }
    }
    if !notices.is_empty() {
        workspace.set_status(format!(
            "{} — using defaults; --safe to skip",
            notices.join("; ")
        ));
    }
}

/// Flattens a multi-line diagnostic into a single status-bar line.
fn one_line(error: &dyn std::fmt::Display) -> String {
    error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `mj describe [NAME]` — Oracle help printed to stdout, read from the live CUA keymap.
///
/// With a name, describes that command (or variable); with none, lists every key binding.
fn run_describe(query: Option<&str>) -> ExitCode {
    let keymap = keymaker::cua();
    let output = match query {
        None => oracle::describe_bindings(&keymap),
        Some(name) if oracle::command_doc(name).is_some() => {
            oracle::describe_function(&keymap, name)
        }
        Some(name) if oracle::variable_doc(name).is_some() => oracle::describe_variable(name),
        Some(name) => oracle::describe_function(&keymap, name), // renders the "no command" notice
    };
    println!("{output}");
    ExitCode::SUCCESS
}

/// `mj apropos <WORD>` — Oracle keyword search printed to stdout.
fn run_apropos(query: Option<&str>) -> ExitCode {
    let Some(word) = query else {
        eprintln!("{PROGRAM}: apropos needs a keyword. Try `{PROGRAM} apropos save`.");
        return ExitCode::FAILURE;
    };
    println!("{}", oracle::apropos(word));
    ExitCode::SUCCESS
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
        assert_eq!(classify(&owned(&["session"])), Action::Session(None));
        assert_eq!(
            classify(&owned(&["session", "clear"])),
            Action::Session(Some("clear".to_owned()))
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

    #[test]
    fn oracle_subcommands_classify() {
        assert_eq!(
            classify(&owned(&["describe", "save"])),
            Action::Describe(Some("save".to_owned()))
        );
        assert_eq!(classify(&owned(&["describe"])), Action::Describe(None));
        assert_eq!(
            classify(&owned(&["apropos", "select"])),
            Action::Apropos(Some("select".to_owned()))
        );
        assert_eq!(
            classify(&owned(&["info", "emacs"])),
            Action::Info(Some("emacs".to_owned()))
        );
        assert_eq!(classify(&owned(&["info"])), Action::Info(None));
    }

    #[test]
    fn every_oracle_command_is_handled_by_the_editor() {
        // Cross-check that Oracle's catalog and `Editor::execute` stay in lockstep: a registered
        // command that `execute` does not handle would set an "unbound command" status.
        use majestic_core::Editor;
        for name in oracle::command_names() {
            let mut editor = Editor::new();
            editor.execute(name);
            assert!(
                !editor.status().starts_with("unbound command"),
                "`{name}` is in Oracle's catalog but not handled by Editor::execute"
            );
        }
    }
}
