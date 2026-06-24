// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Seraph policy decision engine — the fail-closed gate every agent action passes (PRD #1
//! §5.2.4).
//!
//! Seraph turns an [`AgentAction`] the model wants to take into a [`Decision`] — `Allow`,
//! `NeedsApproval` (the user must confirm — diff-approval or an elicitation), or `Deny`. The rules
//! live in a [`Policy`], which deserializes from the Nickel manifest only (Standard §5.4): extensions
//! may *tighten* it but never loosen it. Every default is **closed** — an empty allow-list denies
//! shell and network, and edits require approval — so an absent or partial policy never silently
//! widens what the agent may do.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// An action the agent proposes, presented to [`Policy::decide`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAction {
    /// Apply a buffer edit (the hashline edit format).
    Edit,
    /// Run an OS shell command (the first whitespace token is the program).
    Shell {
        /// The full command line the agent wants to run.
        command: String,
    },
    /// Make a network request to `host`.
    Network {
        /// The host the agent wants to reach (e.g. `"api.github.com"`).
        host: String,
    },
    /// Read the file at `path`.
    ReadPath {
        /// The path the agent wants to read.
        path: PathBuf,
    },
}

/// Seraph's ruling on an [`AgentAction`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The action may proceed without asking the user.
    Allow,
    /// The action may proceed only after the user approves it (diff-approval / confirmation).
    NeedsApproval,
    /// The action is forbidden by policy and must not run.
    Deny {
        /// Why the action was denied — shown to the user and returned to the agent.
        reason: String,
    },
}

impl Decision {
    /// A denial carrying `reason`.
    fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    /// Whether the action is forbidden.
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
}

/// The agent-governance policy. Declarative and **fail-closed**: every field's default forbids or
/// gates the corresponding action, so an empty or partial policy is the safe one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Hosts the agent may reach over the network. Empty (the default) means **no network**.
    pub network_allowlist: Vec<String>,
    /// Program names the agent may run via the shell (matched by basename, e.g. `"cargo"`, `"git"`).
    /// Empty (the default) means **no shell**.
    pub shell_allowlist: Vec<String>,
    /// Whether buffer edits require the user's diff-approval. Default `true` — diff-approval is on, so
    /// the agent can never apply an edit the user did not see.
    pub edits_need_approval: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            network_allowlist: Vec::new(),
            shell_allowlist: Vec::new(),
            edits_need_approval: true,
        }
    }
}

impl Policy {
    /// The default fail-closed policy: no shell, no network, edits need approval.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rules `action`. Unknown or unlisted risky actions are denied; edits are gated by
    /// [`Self::edits_need_approval`]; reads are allowed (the working-directory jail is enforced by the
    /// sandbox, not the policy ruling).
    #[must_use]
    pub fn decide(&self, action: &AgentAction) -> Decision {
        match action {
            AgentAction::Edit => {
                if self.edits_need_approval {
                    Decision::NeedsApproval
                } else {
                    Decision::Allow
                }
            }
            AgentAction::Shell { command } => {
                let program = program_basename(command);
                if program.is_empty() {
                    Decision::deny("empty shell command")
                } else if self
                    .shell_allowlist
                    .iter()
                    .any(|allowed| allowed == program)
                {
                    // Allowed to run, but the user still confirms each invocation.
                    Decision::NeedsApproval
                } else {
                    Decision::deny(format!(
                        "shell program `{program}` is not in the allow-list"
                    ))
                }
            }
            AgentAction::Network { host } => {
                if self.network_allowlist.iter().any(|allowed| allowed == host) {
                    Decision::Allow
                } else {
                    Decision::deny(format!("network host `{host}` is not in the allow-list"))
                }
            }
            // Reads are low-risk; confining them to the project is the sandbox's job, not the ruling.
            AgentAction::ReadPath { .. } => Decision::Allow,
        }
    }
}

/// The program name a shell `command` would run: the basename of its first whitespace token (so
/// `"/usr/bin/cargo test"` matches an allow-list entry `"cargo"`).
fn program_basename(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::{AgentAction, Decision, Policy};
    use std::path::PathBuf;

    fn shell(command: &str) -> AgentAction {
        AgentAction::Shell {
            command: command.to_owned(),
        }
    }

    #[test]
    fn default_policy_is_fail_closed() {
        let policy = Policy::new();
        // Edits need approval; shell and network are denied; reads are allowed.
        assert_eq!(policy.decide(&AgentAction::Edit), Decision::NeedsApproval);
        assert!(policy.decide(&shell("cargo test")).is_denied());
        assert!(policy
            .decide(&AgentAction::Network {
                host: "example.com".to_owned()
            })
            .is_denied());
        assert_eq!(
            policy.decide(&AgentAction::ReadPath {
                path: PathBuf::from("src/main.rs")
            }),
            Decision::Allow
        );
    }

    #[test]
    fn allow_listed_shell_needs_approval_others_denied() {
        let policy = Policy {
            shell_allowlist: vec!["cargo".to_owned(), "git".to_owned()],
            ..Policy::default()
        };
        assert_eq!(policy.decide(&shell("cargo test")), Decision::NeedsApproval);
        // Basename match: an absolute path to an allowed program still passes.
        assert_eq!(
            policy.decide(&shell("/usr/bin/git status")),
            Decision::NeedsApproval
        );
        assert!(policy.decide(&shell("rm -rf /")).is_denied());
    }

    #[test]
    fn allow_listed_host_is_allowed_others_denied() {
        let policy = Policy {
            network_allowlist: vec!["api.github.com".to_owned()],
            ..Policy::default()
        };
        assert_eq!(
            policy.decide(&AgentAction::Network {
                host: "api.github.com".to_owned()
            }),
            Decision::Allow
        );
        assert!(policy
            .decide(&AgentAction::Network {
                host: "evil.example".to_owned()
            })
            .is_denied());
    }

    #[test]
    fn edits_can_be_auto_approved_when_configured() {
        let policy = Policy {
            edits_need_approval: false,
            ..Policy::default()
        };
        assert_eq!(policy.decide(&AgentAction::Edit), Decision::Allow);
    }

    #[test]
    fn deserializes_partial_policy_with_closed_defaults() {
        // A manifest that names only a shell allow-list: network stays empty (denied), edits still
        // need approval — the omitted fields fall back to the fail-closed defaults.
        let policy: Policy = serde_json::from_str(r#"{ "shell_allowlist": ["cargo"] }"#).unwrap();
        assert_eq!(
            policy.decide(&shell("cargo build")),
            Decision::NeedsApproval
        );
        assert_eq!(policy.decide(&AgentAction::Edit), Decision::NeedsApproval);
        assert!(policy
            .decide(&AgentAction::Network {
                host: "anything".to_owned()
            })
            .is_denied());
    }

    #[test]
    fn unknown_policy_keys_are_rejected() {
        // `deny_unknown_fields` catches typos so a misspelled rule can't silently widen the policy.
        let result: Result<Policy, _> = serde_json::from_str(r#"{ "shel_allowlist": ["cargo"] }"#);
        result.unwrap_err();
    }
}
