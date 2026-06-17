// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic Steel runtime — the imperative half of the hybrid configuration (Standard §5; PRD
//! #1 §6.11).
//!
//! Embeds an in-process [Steel](https://github.com/mattwparas/steel) VM and exposes a small,
//! fault-isolated `(majestic …)` API to the user's `config.scm`. The script runs once at
//! startup and applies imperative overrides on top of the declarative Nickel manifest; a syntax
//! or runtime error in the script is contained as a [`ScriptError`] (never a panic), so a broken
//! config cannot brick the editor — the host falls back to the settings gathered so far.
//!
//! The API surface is intentionally tiny in M1 (settings overrides + logging); the full
//! extension surface — command/keymap registration, hooks, and the sandboxed effect-table for
//! agent-authored code — lands at M2/M3.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// The `mj` version, exposed to scripts via `(majestic-version)`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Imperative overrides a `config.scm` accumulated through the `(majestic …)` API.
///
/// Each setting is `Some` only when the script touched it, so the host can layer these onto the
/// Nickel manifest without clobbering untouched fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Indent width override, if `(majestic-set-tab-width! n)` was called.
    pub tab_width: Option<usize>,
    /// Theme override, if `(majestic-set-theme! name)` was called.
    pub theme: Option<String>,
    /// Keymap override, if `(majestic-set-keymap! name)` was called.
    pub keymap: Option<String>,
    /// Messages emitted by `(majestic-log msg)`, in order.
    pub logs: Vec<String>,
}

/// An embedded Steel VM with the `(majestic …)` configuration API registered.
pub struct Runtime {
    engine: Engine,
    settings: Arc<Mutex<Settings>>,
}

impl Runtime {
    /// Creates a runtime with the `(majestic …)` API registered against fresh [`Settings`].
    #[must_use]
    pub fn new() -> Self {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let mut engine = Engine::new();
        register_api(&mut engine, &settings);
        Self { engine, settings }
    }

    /// Evaluates Steel `source`, applying any `(majestic …)` calls to the runtime's settings.
    ///
    /// # Errors
    /// Returns [`ScriptError::Evaluate`] if the script fails to parse or evaluate. Such errors
    /// are contained: the settings gathered before the failure remain readable via
    /// [`Runtime::settings`].
    pub fn run_str(&mut self, source: &str) -> Result<(), ScriptError> {
        // `run` requires an owned program (`Into<Cow<'static, str>>`), so hand it a `String`.
        self.engine
            .run(source.to_owned())
            .map(|_values| ())
            .map_err(|error| ScriptError::Evaluate(error.to_string()))
    }

    /// Reads and evaluates the script at `path`.
    ///
    /// # Errors
    /// Returns [`ScriptError::Read`] if the file cannot be read, or [`ScriptError::Evaluate`] if
    /// it fails to parse or evaluate.
    pub fn run_file(&mut self, path: &Path) -> Result<(), ScriptError> {
        let source = std::fs::read_to_string(path).map_err(ScriptError::Read)?;
        self.run_str(&source)
    }

    /// A snapshot of the overrides accumulated so far.
    #[must_use]
    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// Locates the active `config.scm`, or `None` when none exists.
    ///
    /// Search order: `$MAJESTIC_INIT` (explicit override) → `./config.scm` (project-local) →
    /// `$XDG_CONFIG_HOME/majestic/config.scm` → `$HOME/.config/majestic/config.scm`.
    #[must_use]
    pub fn discover() -> Option<PathBuf> {
        if let Some(explicit) = std::env::var_os("MAJESTIC_INIT") {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                return Some(path);
            }
        }
        let local = PathBuf::from("config.scm");
        if local.is_file() {
            return Some(local);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        let path = base.join("majestic").join("config.scm");
        path.is_file().then_some(path)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The Steel `Engine` is not `Debug`; surface only the accumulated settings.
        f.debug_struct("Runtime")
            .field("settings", &self.settings())
            .finish_non_exhaustive()
    }
}

/// Registers the `(majestic …)` API against `settings`. Each setter mutates the shared state
/// through interior mutability so the closures stay `Fn` (Steel requires `Send + Sync + 'static`).
fn register_api(engine: &mut Engine, settings: &Arc<Mutex<Settings>>) {
    let state = Arc::clone(settings);
    engine.register_fn("majestic-set-tab-width!", move |columns: i64| {
        // Steel integers arrive as i64; a negative width is meaningless, so floor at zero and
        // let the host clamp the upper bound.
        lock(&state).tab_width = Some(usize::try_from(columns).unwrap_or(0));
    });

    let state = Arc::clone(settings);
    engine.register_fn("majestic-set-theme!", move |name: String| {
        lock(&state).theme = Some(name);
    });

    let state = Arc::clone(settings);
    engine.register_fn("majestic-set-keymap!", move |name: String| {
        lock(&state).keymap = Some(name);
    });

    let state = Arc::clone(settings);
    engine.register_fn("majestic-log", move |message: String| {
        lock(&state).logs.push(message);
    });

    engine.register_fn("majestic-version", || VERSION.to_owned());
}

/// Locks `mutex`, recovering the guard if a previous holder panicked (poison) rather than
/// propagating the panic — the config runtime is single-threaded, so contention never occurs.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why running a `config.scm` failed.
#[derive(Debug)]
pub enum ScriptError {
    /// The script file could not be read.
    Read(io::Error),
    /// The script failed to parse or evaluate. The message is the rendered Steel diagnostic.
    Evaluate(String),
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "cannot read config script: {error}"),
            Self::Evaluate(message) => write!(f, "config script error:\n{message}"),
        }
    }
}

impl std::error::Error for ScriptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Evaluate(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Runtime, ScriptError};

    #[test]
    fn sets_tab_width() {
        let mut runtime = Runtime::new();
        runtime.run_str("(majestic-set-tab-width! 2)").unwrap();
        assert_eq!(runtime.settings().tab_width, Some(2));
    }

    #[test]
    fn sets_theme_and_keymap() {
        let mut runtime = Runtime::new();
        runtime
            .run_str(r#"(majestic-set-theme! "steelbore") (majestic-set-keymap! "vim")"#)
            .unwrap();
        let settings = runtime.settings();
        assert_eq!(settings.theme.as_deref(), Some("steelbore"));
        assert_eq!(settings.keymap.as_deref(), Some("vim"));
    }

    #[test]
    fn real_scheme_logic_runs() {
        // The script is a real Steel program, not static data.
        let mut runtime = Runtime::new();
        runtime
            .run_str("(when (> 3 2) (majestic-set-tab-width! 8))")
            .unwrap();
        assert_eq!(runtime.settings().tab_width, Some(8));
    }

    #[test]
    fn version_is_exposed_to_scripts() {
        let mut runtime = Runtime::new();
        runtime
            .run_str("(majestic-log (majestic-version))")
            .unwrap();
        assert_eq!(runtime.settings().logs, vec![env!("CARGO_PKG_VERSION")]);
    }

    #[test]
    fn syntax_error_is_contained() {
        let mut runtime = Runtime::new();
        assert!(matches!(
            runtime.run_str("(this is broken"),
            Err(ScriptError::Evaluate(_))
        ));
    }

    #[test]
    fn unknown_function_fails_atomically() {
        let mut runtime = Runtime::new();
        // Steel compiles the whole program before running, so an unbound identifier fails the
        // script atomically — no partial application — returned as an error, never panicked.
        let result = runtime.run_str("(majestic-set-tab-width! 3) (no-such-majestic-fn 1)");
        assert!(matches!(result, Err(ScriptError::Evaluate(_))));
        assert_eq!(runtime.settings().tab_width, None);
    }
}
