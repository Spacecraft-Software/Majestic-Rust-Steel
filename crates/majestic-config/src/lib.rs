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

use std::collections::BTreeMap;
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
    /// Manifest-pinned extensions, keyed by name. Each declares an exact pinned `version`; the host
    /// loads the enabled ones (in name order) and verifies the pin. Empty by default.
    pub extensions: BTreeMap<String, ExtensionSpec>,
    /// The Seraph agent-governance policy (PRD #1 §5.2.4) — declarative and fail-closed. Omitted, it
    /// is the closed default: no network, no shell, edits require approval. The host hands this to
    /// Seraph as the only source of agent permissions (extensions may tighten it, never loosen it).
    pub seraph: seraph::Policy,
    /// The Architect agent's provider settings (model + base URL). The API key is **not** here — it
    /// comes from the environment (PRD #1 §9).
    pub agent: AgentConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "steelbore".to_owned(),
            keymap: "cua".to_owned(),
            tab_width: DEFAULT_TAB_WIDTH,
            extensions: BTreeMap::new(),
            seraph: seraph::Policy::default(),
            agent: AgentConfig::default(),
        }
    }
}

/// The Architect agent's provider configuration (PRD #1 §5.2.3 / §9), from the manifest's `agent`
/// section. The API key is intentionally absent — it is read from the environment, never the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    /// The model name the provider requests (e.g. `"qwen2.5-coder"`).
    pub model: String,
    /// The OpenAI-compatible base URL (a local Ollama server by default).
    pub base_url: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "qwen2.5-coder".to_owned(),
            base_url: "http://localhost:11434/v1".to_owned(),
        }
    }
}

/// One manifest-pinned extension entry (PRD #1 §5.5 / §6.7).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtensionSpec {
    /// The exact pinned version the extension must declare (Doom/`straight.el`-style reproducibility).
    pub version: String,
    /// Whether to load this extension (`true` by default; set `false` to keep the pin but skip it).
    pub enabled: bool,
    /// An explicit path to the extension's Steel file, overriding the default location. Relative
    /// paths resolve against the manifest's directory.
    pub source: Option<String>,
}

impl Default for ExtensionSpec {
    fn default() -> Self {
        Self {
            version: String::new(),
            enabled: true,
            source: None,
        }
    }
}

impl ExtensionSpec {
    /// The Steel file to load for extension `name`, resolved against `base_dir` (the directory
    /// holding the manifest): the explicit [`Self::source`] when set (joined onto `base_dir`, so an
    /// absolute source is used verbatim), otherwise `base_dir/extensions/<name>.scm`.
    #[must_use]
    pub fn resolve(&self, name: &str, base_dir: &Path) -> PathBuf {
        match &self.source {
            Some(source) => base_dir.join(source),
            None => base_dir.join("extensions").join(format!("{name}.scm")),
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

    /// The enabled extensions in name order (the host loads them in this deterministic order).
    pub fn enabled_extensions(&self) -> impl Iterator<Item = (&str, &ExtensionSpec)> {
        self.extensions
            .iter()
            .filter(|(_, spec)| spec.enabled)
            .map(|(name, spec)| (name.as_str(), spec))
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
        let path = Self::default_path()?;
        path.is_file().then_some(path)
    }

    /// The canonical path to write the user manifest, whether or not it exists yet:
    /// `$XDG_CONFIG_HOME/majestic/majestic.ncl`, else `$HOME/.config/majestic/majestic.ncl`.
    /// `None` when neither variable is set (no home to write to).
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("majestic").join("majestic.ncl"))
    }
}

/// Writes a minimal manifest selecting keybinding profile `keymap` at [`Config::default_path`],
/// creating the directory if needed. Used by the first-run profile selector to persist the choice
/// so later launches read it back. Returns the path written.
///
/// # Errors
/// Returns an error when there is no configuration home to write to (neither `$XDG_CONFIG_HOME`
/// nor `$HOME` is set), or when creating the directory or writing the file fails.
pub fn write_keymap(keymap: &str) -> io::Result<PathBuf> {
    let path = Config::default_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config home to write to"))?;
    write_keymap_to(&path, keymap)?;
    Ok(path)
}

/// Writes the minimal first-run manifest to `path` (creating parent directories). Split out so it
/// is testable without touching the real configuration home.
fn write_keymap_to(path: &Path, keymap: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body =
        format!("# Majestic manifest — created by the first-run profile selector.\n{{\n  keymap = \"{keymap}\",\n}}\n");
    std::fs::write(path, body)
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
    use std::path::Path;

    use super::{write_keymap_to, AgentConfig, Config, ConfigError, ExtensionSpec};
    use seraph::{AgentAction, Decision};

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

    #[test]
    fn no_extensions_by_default() {
        let config = Config::load_str("{}").unwrap();
        assert!(config.extensions.is_empty());
        assert_eq!(config.enabled_extensions().count(), 0);
    }

    #[test]
    fn extensions_parse_with_pinned_versions_and_defaults() {
        let config = Config::load_str(
            r#"{
                extensions = {
                    surround = { version = "1.2.0" },
                    legacy = { version = "0.1.0", enabled = false },
                    custom = { version = "2.0.0", source = "ext/custom.scm" },
                },
            }"#,
        )
        .unwrap();
        assert_eq!(config.extensions.len(), 3);
        // `enabled` defaults to true; `source` defaults to None.
        let surround = &config.extensions["surround"];
        assert_eq!(surround.version, "1.2.0");
        assert!(surround.enabled);
        assert_eq!(surround.source, None);
        assert_eq!(
            config.extensions["custom"].source.as_deref(),
            Some("ext/custom.scm")
        );

        // `enabled_extensions` drops the disabled one and yields name order.
        let enabled: Vec<&str> = config.enabled_extensions().map(|(name, _)| name).collect();
        assert_eq!(enabled, vec!["custom", "surround"]); // "legacy" disabled; sorted by name
    }

    #[test]
    fn extension_version_is_required_by_the_contract() {
        // The schema's `version | String` field has no default, so omitting it is a contract error.
        let error = Config::load_str(r"{ extensions = { x = { enabled = true } } }").unwrap_err();
        assert!(matches!(error, ConfigError::Evaluate(_)));
    }

    #[test]
    fn unknown_extension_field_is_rejected() {
        let error = Config::load_str(r#"{ extensions = { x = { version = "1", oops = 1 } } }"#)
            .unwrap_err();
        assert!(matches!(error, ConfigError::Evaluate(_)));
    }

    #[test]
    fn extension_resolves_to_default_location_or_explicit_source() {
        let base = Path::new("/cfg");
        // No source → <base>/extensions/<name>.scm.
        let default_spec = ExtensionSpec {
            version: "1".to_owned(),
            enabled: true,
            source: None,
        };
        assert_eq!(
            default_spec.resolve("surround", base),
            Path::new("/cfg/extensions/surround.scm")
        );
        // Relative source → joined onto base.
        let relative = ExtensionSpec {
            source: Some("ext/custom.scm".to_owned()),
            ..default_spec.clone()
        };
        assert_eq!(
            relative.resolve("custom", base),
            Path::new("/cfg/ext/custom.scm")
        );
        // Absolute source → used verbatim.
        let absolute = ExtensionSpec {
            source: Some("/opt/x.scm".to_owned()),
            ..default_spec
        };
        assert_eq!(absolute.resolve("x", base), Path::new("/opt/x.scm"));
    }

    #[test]
    fn first_run_manifest_round_trips_the_chosen_keymap() {
        // The first-run selector writes a minimal manifest; it must parse back to that profile.
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-firstrun-{}", std::process::id()));
        path.push("majestic.ncl");
        write_keymap_to(&path, "vim").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.keymap, "vim");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn seraph_policy_defaults_to_fail_closed() {
        // An empty manifest yields the closed default: edits need approval, shell + network denied.
        let config = Config::load_str("{}").unwrap();
        assert_eq!(config.seraph, seraph::Policy::default());
        assert_eq!(
            config.seraph.decide(&AgentAction::Edit),
            Decision::NeedsApproval
        );
        assert!(config
            .seraph
            .decide(&AgentAction::Shell {
                command: "cargo test".to_owned()
            })
            .is_denied());
    }

    #[test]
    fn seraph_section_drives_the_policy() {
        // The manifest's `seraph` allow-lists flow straight into the policy that gates the agent.
        let config = Config::load_str(
            r#"{
                seraph = {
                    shell_allowlist = ["cargo", "git"],
                    network_allowlist = ["api.github.com"],
                    edits_need_approval = false,
                },
            }"#,
        )
        .unwrap();
        assert_eq!(config.seraph.decide(&AgentAction::Edit), Decision::Allow);
        assert_eq!(
            config.seraph.decide(&AgentAction::Shell {
                command: "cargo build".to_owned()
            }),
            Decision::NeedsApproval
        );
        assert_eq!(
            config.seraph.decide(&AgentAction::Network {
                host: "api.github.com".to_owned()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn unknown_seraph_field_is_rejected() {
        // A typo inside the `seraph` section is caught (Nickel contract + serde deny_unknown_fields).
        let error = Config::load_str(r#"{ seraph = { shel_allowlist = ["cargo"] } }"#).unwrap_err();
        assert!(matches!(error, ConfigError::Evaluate(_)));
    }

    #[test]
    fn agent_section_drives_provider_settings() {
        let config = Config::load_str(
            r#"{ agent = { model = "llama3.2", base_url = "http://host:1234/v1" } }"#,
        )
        .unwrap();
        assert_eq!(config.agent.model, "llama3.2");
        assert_eq!(config.agent.base_url, "http://host:1234/v1");
    }

    #[test]
    fn agent_section_defaults_to_local_ollama_when_omitted() {
        let config = Config::load_str("{}").unwrap();
        assert_eq!(config.agent, AgentConfig::default());
        assert_eq!(config.agent.model, "qwen2.5-coder");
    }
}
