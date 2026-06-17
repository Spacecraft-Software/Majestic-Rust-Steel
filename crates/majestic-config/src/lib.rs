// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic configuration — the declarative half of the hybrid manifest (Standard §5; PRD #1
//! §6.10).
//!
//! The user writes a [Nickel](https://nickel-lang.org/) manifest (`majestic.ncl`); this crate
//! merges it onto a bundled schema contract ([`schema.ncl`](../src/schema.ncl)) so omitted
//! fields fall back to defaults and every field is type-checked, then deserializes the result
//! into a typed [`Config`]. Evaluation is total and fail-soft at the call site: a malformed
//! manifest yields a [`ConfigError`] rather than a panic, and the host starts from
//! [`Config::default`] (the basis of **safe mode**, where the manifest is skipped entirely).
//!
//! The imperative half — a Steel `config.scm` with the `(majestic …)` API — lands in
//! `majestic-steel`; this crate owns only the validated settings record.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The bundled schema contract; merged onto the user manifest to supply defaults and types.
const SCHEMA: &str = include_str!("schema.ncl");

/// Default indent width in columns (CUA editors conventionally indent by four).
const DEFAULT_TAB_WIDTH: usize = 4;

/// The validated Majestic settings, produced by evaluating the Nickel manifest.
///
/// Fields mirror `schema.ncl`. Unknown keys are rejected (`deny_unknown_fields`) so a typo in
/// the manifest is reported rather than silently ignored; omitted keys take the schema default.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Theme token set selecting the palette (e.g. `"steelbore"`).
    pub theme: String,
    /// Keybinding profile name (e.g. `"cua"`).
    pub keymap: String,
    /// Indent width in columns, as written in the manifest (clamp on use via [`Config::tab_width`]).
    pub tab_width: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "steelbore".to_owned(),
            keymap: "cua".to_owned(),
            tab_width: DEFAULT_TAB_WIDTH,
        }
    }
}

impl Config {
    /// The indent width, clamped to a sane `1..=16` range so a hostile or mistaken manifest
    /// cannot force pathological allocations when the editor builds an indent string.
    #[must_use]
    pub fn tab_width(&self) -> usize {
        self.tab_width.clamp(1, 16)
    }

    /// Loads and validates the manifest at `path`.
    ///
    /// # Errors
    /// Returns [`ConfigError::Read`] if the file cannot be read, or [`ConfigError::Evaluate`]
    /// if the Nickel program fails to evaluate, type-check, or deserialize into [`Config`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::load_str(&source)
    }

    /// Loads and validates a manifest from in-memory Nickel `source`.
    ///
    /// # Errors
    /// Returns [`ConfigError::Evaluate`] if the program fails to evaluate, type-check, or
    /// deserialize.
    pub fn load_str(source: &str) -> Result<Self, ConfigError> {
        // Merge the user manifest onto the schema record: `(schema) & (user)`. Nickel merge
        // lets user values override the schema defaults while the schema's field contracts
        // still validate them. Both sides are parenthesised so each is a single expression.
        let program = format!("({SCHEMA})\n& (\n{source}\n)");
        nickel_lang_core::deserialize::from_str::<Self>(&program)
            .map_err(|error| ConfigError::Evaluate(error.to_string()))
    }

    /// Locates the active manifest, or `None` when no configuration exists (use defaults).
    ///
    /// Search order: `$MAJESTIC_CONFIG` (explicit override) → `./majestic.ncl` (project-local)
    /// → `$XDG_CONFIG_HOME/majestic/majestic.ncl` → `$HOME/.config/majestic/majestic.ncl`.
    #[must_use]
    pub fn discover() -> Option<PathBuf> {
        if let Some(explicit) = std::env::var_os("MAJESTIC_CONFIG") {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                return Some(path);
            }
        }
        let local = PathBuf::from("majestic.ncl");
        if local.is_file() {
            return Some(local);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        let path = base.join("majestic").join("majestic.ncl");
        path.is_file().then_some(path)
    }
}

/// Why loading a [`Config`] failed.
#[derive(Debug)]
pub enum ConfigError {
    /// The manifest file could not be read.
    Read(io::Error),
    /// The Nickel program failed to evaluate, type-check, or deserialize. The message is the
    /// rendered Nickel diagnostic.
    Evaluate(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "cannot read configuration: {error}"),
            Self::Evaluate(message) => write!(f, "invalid configuration:\n{message}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Evaluate(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError};

    #[test]
    fn empty_manifest_yields_defaults() {
        let config = Config::load_str("{}").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.tab_width(), 4);
    }

    #[test]
    fn user_values_override_schema_defaults() {
        let config = Config::load_str(r#"{ theme = "steelbore", tab_width = 2 }"#).unwrap();
        assert_eq!(config.tab_width, 2);
        assert_eq!(config.keymap, "cua"); // untouched -> default
    }

    #[test]
    fn nickel_expressions_are_evaluated() {
        // The manifest is a real Nickel program, not static data.
        let config = Config::load_str("let base = 2 in { tab_width = base * 2 }").unwrap();
        assert_eq!(config.tab_width, 4);
    }

    #[test]
    fn wrong_type_is_rejected_by_the_contract() {
        // `tab_width` must satisfy the schema's `Number` contract.
        let error = Config::load_str(r#"{ tab_width = "wide" }"#).unwrap_err();
        assert!(matches!(error, ConfigError::Evaluate(_)));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let error = Config::load_str("{ noSuchOption = 1 }").unwrap_err();
        assert!(matches!(error, ConfigError::Evaluate(_)));
    }

    #[test]
    fn tab_width_is_clamped_to_a_sane_range() {
        let config = Config::load_str("{ tab_width = 9000 }").unwrap();
        assert_eq!(config.tab_width, 9000); // stored verbatim
        assert_eq!(config.tab_width(), 16); // clamped on use
    }
}
