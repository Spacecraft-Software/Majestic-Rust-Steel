// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The governed agent loop (PRD #1 §5.2.3 / §5.2.4) — where the model's tool calls meet Seraph.
//!
//! [`run_turn`] drives one agent turn: it asks the [`Provider`] for a reply and, for every tool the
//! model wants to run, **gates the call through Seraph before it happens** — [`Tools::action`]
//! classifies the call, [`Policy::decide`] rules on it, a `Deny` is reported back to the model
//! without running anything, a `NeedsApproval` is put to the [`Approver`] (the diff dialog in the UI),
//! and only an allowed-or-approved call reaches [`Tools::run`]. Every decision is written to the audit
//! log, and the kill switch is checked at every step so the user can stop the agent within the
//! ≤100 ms budget. No agent side effect reaches a document or the outside world except through this
//! gate. The policy, audit log, and kill switch travel together as a [`Governor`].
//!
//! The loop owns none of the I/O: tool execution sits behind the [`Tools`] trait, which the host
//! implements over the real workspace (tagged reads, hashline edits, sandboxed shell). That keeps the
//! loop pure enough to verify end-to-end with a [`crate::MockProvider`] and a mock `Tools` — no live
//! model required.

use jiff::Timestamp;
use seraph::{AgentAction, AuditLog, Decision, KillSwitch, Policy};

use crate::provider::{CompletionRequest, Message, Provider, ProviderError, ToolCall};

/// Classifies and runs the agent's tool calls. The host implements this over the real workspace
/// (read via `majestic-core`'s tagged read, edit via the hashline `apply`, shell via a sandbox);
/// tests use a mock.
pub trait Tools {
    /// The Seraph action `call` represents, so [`Policy::decide`] can rule on it before it runs.
    fn action(&self, call: &ToolCall) -> AgentAction;

    /// Runs an already-approved `call`, returning output to feed back to the model.
    ///
    /// # Errors
    /// Returns an error message (to be shown to the model) when the tool cannot complete — a bad
    /// argument, a missing file, a stale hashline tag, a failed subprocess, and the like.
    fn run(&mut self, call: &ToolCall) -> Result<String, String>;
}

/// The user's ruling on a [`Decision::NeedsApproval`] tool call (the Apply / Edit / Reject card).
#[derive(Clone, Debug, PartialEq)]
pub enum Approval {
    /// Run the call unchanged.
    Run,
    /// Do not run it.
    Reject,
    /// Run this modified call instead — the user edited the proposed change before applying it.
    RunModified(ToolCall),
}

/// Decides the [`Decision::NeedsApproval`] cases — the Apply/Edit/Reject dialog in the UI, a scripted
/// answer in tests.
pub trait Approver {
    /// How the user wishes to proceed with `call`: run it, reject it, or run an edited version.
    fn approve(&mut self, call: &ToolCall) -> Approval;
}

/// The Seraph governance context threaded through a turn: the policy to rule by, the audit log to
/// record every decision to, and the kill switch to honor. Bundling them keeps [`run_turn`]'s
/// signature honest and mirrors how the host holds them for the whole agent session.
#[derive(Debug)]
pub struct Governor<'a> {
    policy: &'a Policy,
    audit: &'a mut AuditLog,
    kill: &'a KillSwitch,
}

impl<'a> Governor<'a> {
    /// Bundles the `policy`, `audit` log, and `kill` switch for a turn.
    pub fn new(policy: &'a Policy, audit: &'a mut AuditLog, kill: &'a KillSwitch) -> Self {
        Self {
            policy,
            audit,
            kill,
        }
    }
}

/// How a governed agent turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The model finished; the payload is its final assistant message.
    Done(String),
    /// The kill switch was engaged, so the turn halted.
    Stopped,
    /// The turn hit its step budget without the model finishing.
    BudgetExhausted,
    /// The provider failed.
    Failed(ProviderError),
}

/// Runs one governed agent turn over `request`, up to `max_steps` model rounds.
///
/// Each round: the kill switch is checked (engaged → [`Outcome::Stopped`]); the [`Provider`] is asked
/// for a reply; if it makes no tool calls the turn is [`Outcome::Done`] with its text. Otherwise every
/// tool call is gated — [`Tools::action`] classifies it, [`Policy::decide`] rules: `Deny` tells the
/// model and runs nothing; `NeedsApproval` defers to `approver`; an allowed or approved call runs via
/// [`Tools::run`]. Each decision is recorded to the governor's audit log with a timestamp from `now`,
/// the result is appended to the conversation, and the loop continues. Running out of rounds yields
/// [`Outcome::BudgetExhausted`].
pub fn run_turn(
    provider: &dyn Provider,
    tools: &mut dyn Tools,
    approver: &mut dyn Approver,
    governor: &mut Governor<'_>,
    now: impl FnMut() -> Timestamp,
    request: CompletionRequest,
    max_steps: usize,
) -> Outcome {
    run_turn_streaming(
        provider,
        tools,
        approver,
        governor,
        now,
        request,
        max_steps,
        &mut |_| {},
    )
}

/// Like [`run_turn`], but streams assistant text: `on_token` is called with each chunk of the model's
/// reply as it arrives (via [`Provider::complete_streaming`]), so the host can render the answer live.
/// The accumulated reply is still returned as the [`Outcome`], so callers that ignore tokens behave
/// exactly as [`run_turn`].
#[expect(
    clippy::too_many_arguments,
    reason = "the governed loop's inputs (provider, tools, approver, governor, clock, request, budget, token sink) are each distinct and intrinsic; bundling them would only hide the dependencies"
)]
pub fn run_turn_streaming(
    provider: &dyn Provider,
    tools: &mut dyn Tools,
    approver: &mut dyn Approver,
    governor: &mut Governor<'_>,
    mut now: impl FnMut() -> Timestamp,
    mut request: CompletionRequest,
    max_steps: usize,
    on_token: &mut dyn FnMut(&str),
) -> Outcome {
    for _ in 0..max_steps {
        if governor.kill.is_engaged() {
            return Outcome::Stopped;
        }
        let response = match provider.complete_streaming(&request, on_token) {
            Ok(response) => response,
            Err(error) => return Outcome::Failed(error),
        };
        if !response.wants_tools() {
            return Outcome::Done(response.content);
        }

        // The model's tool-calling turn joins the conversation, then each call is gated in order.
        request
            .messages
            .push(Message::assistant(response.content.clone()));
        for call in &response.tool_calls {
            if governor.kill.is_engaged() {
                return Outcome::Stopped;
            }
            let content = gate_and_run(call, governor, tools, approver, &mut now);
            request.messages.push(Message::tool(&call.id, content));
        }
    }
    Outcome::BudgetExhausted
}

/// Gates one tool call through Seraph and runs it if permitted, returning the text to feed back to the
/// model and recording the decision to the governor's audit log.
fn gate_and_run(
    call: &ToolCall,
    governor: &mut Governor<'_>,
    tools: &mut dyn Tools,
    approver: &mut dyn Approver,
    now: &mut impl FnMut() -> Timestamp,
) -> String {
    let action = tools.action(call);
    match governor.policy.decide(&action) {
        Decision::Deny { reason } => {
            governor
                .audit
                .append(now(), format!("denied tool `{}`: {reason}", call.name));
            format!("denied by policy: {reason}")
        }
        Decision::NeedsApproval => match approver.approve(call) {
            Approval::Reject => {
                governor
                    .audit
                    .append(now(), format!("user rejected tool `{}`", call.name));
                "rejected by the user".to_owned()
            }
            Approval::Run => run_tool(call, governor, tools, now),
            Approval::RunModified(modified) => {
                governor
                    .audit
                    .append(now(), format!("user edited tool `{}`", call.name));
                run_tool(&modified, governor, tools, now)
            }
        },
        Decision::Allow => run_tool(call, governor, tools, now),
    }
}

/// Runs an approved or allowed `call` via [`Tools::run`], recording success or failure to the audit
/// log and returning the text to feed back to the model.
fn run_tool(
    call: &ToolCall,
    governor: &mut Governor<'_>,
    tools: &mut dyn Tools,
    now: &mut impl FnMut() -> Timestamp,
) -> String {
    match tools.run(call) {
        Ok(output) => {
            governor
                .audit
                .append(now(), format!("ran tool `{}`", call.name));
            output
        }
        Err(message) => {
            governor
                .audit
                .append(now(), format!("tool `{}` failed: {message}", call.name));
            format!("tool error: {message}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_turn, run_turn_streaming, Approval, Approver, Governor, Outcome, Tools};
    use crate::provider::{CompletionRequest, CompletionResponse, Message, MockProvider, ToolCall};
    use jiff::Timestamp;
    use seraph::{AgentAction, AuditLog, KillSwitch, Policy};
    use serde_json::json;
    use std::path::PathBuf;

    /// A constant audit timestamp (the chain does not require distinct times).
    fn now() -> Timestamp {
        "2026-06-24T10:00:00Z".parse().expect("valid timestamp")
    }

    /// A mock tool surface: every call classifies to `action`, and running records the call name.
    struct MockTools {
        action: AgentAction,
        ran: Vec<String>,
        result: Result<String, String>,
    }

    impl Tools for MockTools {
        fn action(&self, _call: &ToolCall) -> AgentAction {
            self.action.clone()
        }
        fn run(&mut self, call: &ToolCall) -> Result<String, String> {
            self.ran.push(call.name.clone());
            self.result.clone()
        }
    }

    /// A mock approver that answers every prompt the same way.
    struct MockApprover(Approval);
    impl Approver for MockApprover {
        fn approve(&mut self, _call: &ToolCall) -> Approval {
            self.0.clone()
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest::new().with_message(Message::user("do the thing"))
    }

    #[test]
    fn an_allowed_tool_runs_then_the_model_finishes() {
        // Round 1: the model calls a read tool (the policy allows reads); round 2: it answers.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("read")]),
            CompletionResponse::text("here is the answer"),
        ]);
        let mut tools = MockTools {
            action: AgentAction::ReadPath {
                path: PathBuf::from("foo.rs"),
            },
            ran: Vec::new(),
            result: Ok("file contents".to_owned()),
        };
        let mut approver = MockApprover(Approval::Reject); // not needed: reads are allowed outright
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("here is the answer".to_owned()));
        assert_eq!(tools.ran, vec!["read"], "the allowed tool ran");
        assert_eq!(audit.len(), 1);
        assert!(audit.entries()[0].action.contains("ran tool `read`"));
        // The tool result was fed back to the model on the second request.
        let second = &provider.requests()[1];
        assert!(second.messages.iter().any(|m| m.content == "file contents"));
    }

    #[test]
    fn a_denied_tool_never_runs_and_the_model_is_told() {
        // The model tries to run a shell command; the default policy denies shell.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("shell")]),
            CompletionResponse::text("ok, I won't"),
        ]);
        let mut tools = MockTools {
            action: AgentAction::Shell {
                command: "rm -rf /".to_owned(),
            },
            ran: Vec::new(),
            result: Ok("should never run".to_owned()),
        };
        let mut approver = MockApprover(Approval::Run);
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("ok, I won't".to_owned()));
        assert!(tools.ran.is_empty(), "the denied tool must not run");
        assert!(audit.entries()[0].action.contains("denied tool `shell`"));
        // The model saw the denial as the tool result.
        let second = &provider.requests()[1];
        assert!(second
            .messages
            .iter()
            .any(|m| m.content.contains("denied by policy")));
    }

    #[test]
    fn a_needs_approval_tool_is_rejected_when_the_user_declines() {
        // An edit needs approval; the user declines, so it does not run.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("edit")]),
            CompletionResponse::text("understood"),
        ]);
        let mut tools = MockTools {
            action: AgentAction::Edit,
            ran: Vec::new(),
            result: Ok("edited".to_owned()),
        };
        let mut approver = MockApprover(Approval::Reject); // user rejects
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("understood".to_owned()));
        assert!(tools.ran.is_empty(), "a rejected edit must not run");
        assert!(audit.entries()[0]
            .action
            .contains("user rejected tool `edit`"));
    }

    #[test]
    fn a_needs_approval_tool_runs_when_the_user_approves() {
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("edit")]),
            CompletionResponse::text("done"),
        ]);
        let mut tools = MockTools {
            action: AgentAction::Edit,
            ran: Vec::new(),
            result: Ok("edited".to_owned()),
        };
        let mut approver = MockApprover(Approval::Run); // user approves
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("done".to_owned()));
        assert_eq!(tools.ran, vec!["edit"], "an approved edit runs");
    }

    #[test]
    fn a_needs_approval_tool_runs_the_user_modified_call() {
        // The Edit option: the user approves a *modified* call, and that one runs (not the original).
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("edit")]),
            CompletionResponse::text("done"),
        ]);
        let mut tools = MockTools {
            action: AgentAction::Edit,
            ran: Vec::new(),
            result: Ok("edited".to_owned()),
        };
        let modified = ToolCall {
            id: "edit-1".to_owned(),
            name: "edit-modified".to_owned(),
            arguments: json!({}),
        };
        let mut approver = MockApprover(Approval::RunModified(modified));
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("done".to_owned()));
        assert_eq!(
            tools.ran,
            vec!["edit-modified"],
            "the user's modified call ran, not the original"
        );
        assert!(audit.entries()[0].action.contains("user edited tool"));
    }

    #[test]
    fn the_kill_switch_halts_the_turn() {
        let provider = MockProvider::new([CompletionResponse::text("never reached")]);
        let mut tools = MockTools {
            action: AgentAction::Edit,
            ran: Vec::new(),
            result: Ok(String::new()),
        };
        let mut approver = MockApprover(Approval::Run);
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();
        kill.engage(); // already engaged before the first step

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Stopped);
        assert!(provider.requests().is_empty(), "no model call after a stop");
    }

    #[test]
    fn the_step_budget_bounds_a_runaway_loop() {
        // A provider that always asks for another tool call would loop forever; max_steps stops it.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("read")]),
            CompletionResponse::calling("", vec![call("read")]),
            CompletionResponse::calling("", vec![call("read")]),
        ]);
        let mut tools = MockTools {
            action: AgentAction::ReadPath {
                path: PathBuf::from("x"),
            },
            ran: Vec::new(),
            result: Ok("more".to_owned()),
        };
        let mut approver = MockApprover(Approval::Run);
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                2,
            )
        };

        assert_eq!(outcome, Outcome::BudgetExhausted);
        assert_eq!(provider.requests().len(), 2, "exactly max_steps rounds ran");
    }

    #[test]
    fn run_turn_streaming_emits_assistant_text_as_tokens() {
        // The default (non-streaming) provider emits the whole reply as one chunk via `on_token`; the
        // accumulated text matches the returned Outcome, so the host can render live then commit.
        let provider = MockProvider::new([CompletionResponse::text("hello world")]);
        let mut tools = MockTools {
            action: AgentAction::Edit,
            ran: Vec::new(),
            result: Ok(String::new()),
        };
        let mut approver = MockApprover(Approval::Run);
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let mut streamed = String::new();
        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn_streaming(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
                &mut |token| streamed.push_str(token),
            )
        };

        assert_eq!(outcome, Outcome::Done("hello world".to_owned()));
        assert_eq!(
            streamed, "hello world",
            "tokens reassemble to the full reply"
        );
    }

    // ── M3 exit criteria ───────────────────────────────────────────────────────────────────────
    // These two tests are the governance bar for M3: no guarded agent action reaches `Tools::run`
    // except through Seraph, and the audit log records every decision so the run is reconstructable.

    /// A tool surface that classifies by tool name and records every call that actually reaches
    /// [`Tools::run`], so a test can prove a denied or rejected call never executed.
    struct ClassifyingTools {
        ran: Vec<String>,
    }

    impl Tools for ClassifyingTools {
        fn action(&self, call: &ToolCall) -> AgentAction {
            match call.name.as_str() {
                "edit" => AgentAction::Edit,
                "shell" => AgentAction::Shell {
                    command: "rm -rf /".to_owned(),
                },
                _ => AgentAction::ReadPath {
                    path: PathBuf::from("f.rs"),
                },
            }
        }
        fn run(&mut self, call: &ToolCall) -> Result<String, String> {
            self.ran.push(call.name.clone());
            Ok(format!("ran {}", call.name))
        }
    }

    #[test]
    fn red_team_no_guarded_action_bypasses_the_gate() {
        // An adversarial model fires a barrage of guarded calls across rounds. Seraph gates by ACTION
        // classification, not by anything the model or a tool result says, so no amount of model
        // misbehaviour (the moral equivalent of prompt injection) can run a denied or rejected call.
        // Default policy: `shell` denied, `edit` needs approval (the approver rejects every one),
        // `read` allowed. Only the reads may ever reach `run`.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("shell"), call("edit"), call("read")]),
            CompletionResponse::calling("", vec![call("edit"), call("shell")]),
            CompletionResponse::text("done"),
        ]);
        let mut tools = ClassifyingTools { ran: Vec::new() };
        let mut approver = MockApprover(Approval::Reject); // reject every edit that needs approval
        let policy = Policy::default(); // fail-closed
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(outcome, Outcome::Done("done".to_owned()));
        assert!(
            tools.ran.iter().all(|name| name == "read"),
            "a guarded action bypassed the gate and ran: {:?}",
            tools.ran
        );
        assert_eq!(tools.ran.len(), 1, "exactly the one allowed read ran");
    }

    #[test]
    fn the_audit_records_every_gated_decision_for_reconstruction() {
        // Every tool call — denied, rejected, or run — must leave one audit entry, in order, so the
        // log alone reconstructs exactly what the agent did and what the gate decided.
        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![call("shell"), call("edit"), call("read")]),
            CompletionResponse::text("done"),
        ]);
        let mut tools = ClassifyingTools { ran: Vec::new() };
        let mut approver = MockApprover(Approval::Reject);
        let policy = Policy::default();
        let mut audit = AuditLog::new();
        let kill = KillSwitch::new();

        let _outcome = {
            let mut governor = Governor::new(&policy, &mut audit, &kill);
            run_turn(
                &provider,
                &mut tools,
                &mut approver,
                &mut governor,
                now,
                request(),
                8,
            )
        };

        assert_eq!(audit.len(), 3, "one entry per tool call");
        let actions: Vec<&str> = audit.entries().iter().map(|e| e.action.as_str()).collect();
        assert!(actions[0].contains("denied tool `shell`"), "{actions:?}");
        assert!(
            actions[1].contains("user rejected tool `edit`"),
            "{actions:?}"
        );
        assert!(actions[2].contains("ran tool `read`"), "{actions:?}");
    }
}
