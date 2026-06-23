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

use std::collections::BTreeMap;
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
    /// Versions declared by loaded extensions via `(majestic-provides! name version)`, keyed by
    /// extension name. The host checks each against the manifest's pin.
    pub provided: BTreeMap<String, String>,
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

    /// Loads a manifest-pinned extension: runs the Steel file at `path` on this runtime, then
    /// verifies the version it declared via `(majestic-provides! name version)` equals
    /// `pinned_version`. Extensions share the runtime (so their registrations are visible to
    /// `config.scm` and to one another), and are fault-isolated like any script.
    ///
    /// # Errors
    /// Returns [`ExtensionError::Script`] if the file cannot be read or fails to evaluate,
    /// [`ExtensionError::Undeclared`] if it never called `(majestic-provides! name …)`, or
    /// [`ExtensionError::VersionMismatch`] if the declared version differs from the pin.
    pub fn load_extension(
        &mut self,
        name: &str,
        pinned_version: &str,
        path: &Path,
    ) -> Result<(), ExtensionError> {
        self.run_file(path).map_err(ExtensionError::Script)?;
        match lock(&self.settings).provided.get(name) {
            None => Err(ExtensionError::Undeclared {
                name: name.to_owned(),
            }),
            Some(actual) if actual != pinned_version => Err(ExtensionError::VersionMismatch {
                name: name.to_owned(),
                pinned: pinned_version.to_owned(),
                actual: actual.clone(),
            }),
            Some(_) => Ok(()),
        }
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

    let state = Arc::clone(settings);
    engine.register_fn(
        "majestic-provides!",
        move |name: String, version: String| {
            // An extension declares its identity + version; the host verifies this against the pin.
            lock(&state).provided.insert(name, version);
        },
    );

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

/// Why loading a manifest-pinned extension failed.
#[derive(Debug)]
pub enum ExtensionError {
    /// The extension's Steel file could not be read or failed to evaluate.
    Script(ScriptError),
    /// The extension ran but never declared itself via `(majestic-provides! name …)`, so its
    /// version cannot be verified against the pin.
    Undeclared {
        /// The extension name (as pinned in the manifest).
        name: String,
    },
    /// The version the extension declared differs from the manifest's pin.
    VersionMismatch {
        /// The extension name.
        name: String,
        /// The version pinned in the manifest.
        pinned: String,
        /// The version the extension actually declared.
        actual: String,
    },
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Script(error) => write!(f, "{error}"),
            Self::Undeclared { name } => {
                write!(
                    f,
                    "extension `{name}` did not declare a version via majestic-provides!"
                )
            }
            Self::VersionMismatch {
                name,
                pinned,
                actual,
            } => write!(
                f,
                "extension `{name}` is pinned to {pinned} but declares {actual}"
            ),
        }
    }
}

impl std::error::Error for ExtensionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Script(error) => Some(error),
            Self::Undeclared { .. } | Self::VersionMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtensionError, Runtime, ScriptError};

    /// Writes `source` to a uniquely-named temp `.scm` and returns its path (caller removes it).
    fn temp_scm(tag: &str, source: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "majestic-ext-{}-{tag}-{}.scm",
            std::process::id(),
            tag.len()
        ));
        std::fs::write(&path, source).unwrap();
        path
    }

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

    #[test]
    fn provides_records_the_declared_version() {
        let mut runtime = Runtime::new();
        runtime
            .run_str(r#"(majestic-provides! "surround" "1.2.0")"#)
            .unwrap();
        assert_eq!(
            runtime
                .settings()
                .provided
                .get("surround")
                .map(String::as_str),
            Some("1.2.0")
        );
    }

    #[test]
    fn load_extension_succeeds_on_a_matching_pin() {
        let path = temp_scm(
            "ok",
            r#"(majestic-provides! "surround" "1.2.0") (majestic-set-tab-width! 2)"#,
        );
        let mut runtime = Runtime::new();
        let result = runtime.load_extension("surround", "1.2.0", &path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_ok(), "matching pin should load: {result:?}");
        // The extension's settings took effect on the shared runtime.
        assert_eq!(runtime.settings().tab_width, Some(2));
    }

    #[test]
    fn load_extension_rejects_a_version_mismatch() {
        let path = temp_scm("mismatch", r#"(majestic-provides! "surround" "9.9.9")"#);
        let mut runtime = Runtime::new();
        let result = runtime.load_extension("surround", "1.2.0", &path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            result,
            Err(ExtensionError::VersionMismatch { actual, .. }) if actual == "9.9.9"
        ));
    }

    #[test]
    fn load_extension_rejects_an_undeclared_extension() {
        // Runs fine but never calls majestic-provides!, so its version cannot be verified.
        let path = temp_scm("undeclared", "(majestic-set-tab-width! 2)");
        let mut runtime = Runtime::new();
        let result = runtime.load_extension("surround", "1.2.0", &path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Err(ExtensionError::Undeclared { name }) if name == "surround"));
    }

    #[test]
    fn load_extension_propagates_a_missing_file() {
        let mut runtime = Runtime::new();
        let result =
            runtime.load_extension("x", "1.0.0", std::path::Path::new("/no/such/ext-xyz.scm"));
        assert!(matches!(
            result,
            Err(ExtensionError::Script(ScriptError::Read(_)))
        ));
    }
}
