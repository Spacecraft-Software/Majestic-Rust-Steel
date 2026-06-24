// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`AgentSession`] — the configured agent the host drives.
//!
//! A session bundles the long-lived pieces of a governed agent: the [`Provider`], the Seraph
//! [`Policy`] and [`KillSwitch`], an [`AuditLog`] that persists across turns, and the system prompt
//! plus tool specs. [`AgentSession::run`] drives one user turn against a buffer — building the
//! request, pointing [`BufferTools`] at the buffer, and handing everything to
//! [`architect::run_turn`], which gates every tool call through Seraph. This is the single call the
//! UI makes; the off-thread runner that keeps the UI responsive layers on top later.

use architect::{
    run_turn, Approver, CompletionRequest, Governor, Message, Outcome, Provider, ToolSpec,
};
use majestic_core::Buffer;
use seraph::{AuditLog, KillSwitch, Policy};

use crate::tools::{buffer_tool_specs, BufferTools};

/// A configured governed agent. Holds the provider, the Seraph policy + kill switch, the running
/// audit log, and the system prompt + advertised tools.
#[derive(Debug)]
pub struct AgentSession {
    provider: Box<dyn Provider>,
    policy: Policy,
    kill: KillSwitch,
    audit: AuditLog,
    system_prompt: String,
    tools: Vec<ToolSpec>,
    max_steps: usize,
}

/// The default ceiling on model rounds in one turn, so a misbehaving model cannot loop forever.
const DEFAULT_MAX_STEPS: usize = 16;

impl AgentSession {
    /// Builds a session from a `provider`, the Seraph `policy`, and a `system_prompt`, advertising the
    /// standard [`buffer_tool_specs`] (read + edit) and a fresh kill switch and audit log.
    #[must_use]
    pub fn new(
        provider: Box<dyn Provider>,
        policy: Policy,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            policy,
            kill: KillSwitch::new(),
            audit: AuditLog::new(),
            system_prompt: system_prompt.into(),
            tools: buffer_tool_specs(),
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// A clone of the kill switch — engage it from the UI to stop the agent (`agent-stop-all`).
    #[must_use]
    pub fn kill_switch(&self) -> KillSwitch {
        self.kill.clone()
    }

    /// The audit log accumulated so far (every gated decision across every turn).
    #[must_use]
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Runs one turn: the model answers `user_message`, reading and editing `buffer` through tools that
    /// Seraph gates (prompting `approver` when a call needs approval). Edits go through the hashline
    /// path, so a stale tag is rejected and the model is told to re-read.
    pub fn run(
        &mut self,
        buffer: &mut Buffer,
        approver: &mut dyn Approver,
        user_message: &str,
    ) -> Outcome {
        let mut request = CompletionRequest::new()
            .with_message(Message::system(self.system_prompt.clone()))
            .with_message(Message::user(user_message));
        for tool in &self.tools {
            request = request.with_tool(tool.clone());
        }

        let mut tools = BufferTools::new(buffer);
        let mut governor = Governor::new(&self.policy, &mut self.audit, &self.kill);
        run_turn(
            self.provider.as_ref(),
            &mut tools,
            approver,
            &mut governor,
            jiff::Timestamp::now,
            request,
            self.max_steps,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::AgentSession;
    use architect::{Approver, CompletionResponse, MockProvider, Outcome, ToolCall};
    use majestic_core::{tagged_read, Buffer};
    use seraph::Policy;
    use serde_json::{json, Value};

    /// An approver with a fixed answer.
    struct FixedApprover(bool);
    impl Approver for FixedApprover {
        fn approve(&mut self, _call: &ToolCall) -> bool {
            self.0
        }
    }

    fn tool_call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_owned(),
            arguments,
        }
    }

    fn tag_of(buffer: &Buffer, line: usize) -> String {
        let read = tagged_read(buffer);
        let row = read.lines().nth(line).expect("line present");
        let after_colon = row.split_once(':').expect("colon").1;
        after_colon.split_once('│').expect("separator").0.to_owned()
    }

    #[test]
    fn the_full_stack_edits_a_real_buffer() {
        // The capstone: a scripted model reads then edits a REAL buffer through the whole gated stack
        // (provider -> loop -> Seraph policy -> BufferTools -> hashline -> buffer mutation).
        let mut buffer = Buffer::from_text("fn main() {}\n");
        let tag = tag_of(&buffer, 0); // line 1's tag, as the model would read it

        let provider = MockProvider::new([
            CompletionResponse::calling("", vec![tool_call("read", json!({}))]),
            CompletionResponse::calling(
                "",
                vec![tool_call(
                    "edit",
                    json!({ "edits": [{
                        "line": 1, "tag": tag, "op": "replace",
                        "text": "fn main() { println!(\"hi\"); }"
                    }] }),
                )],
            ),
            CompletionResponse::text("done"),
        ]);
        // Auto-approve edits so the gate allows them outright.
        let policy = Policy {
            edits_need_approval: false,
            ..Policy::default()
        };
        let mut session = AgentSession::new(Box::new(provider), policy, "You edit code.");
        let mut approver = FixedApprover(false); // never consulted: edits are auto-approved

        let outcome = session.run(&mut buffer, &mut approver, "add a hello print");

        assert_eq!(outcome, Outcome::Done("done".to_owned()));
        assert!(
            buffer.text().contains("println!"),
            "the buffer was edited through the governed stack: {}",
            buffer.text()
        );
        // Every gated decision was recorded.
        assert!(session.audit().len() >= 2);
    }

    #[test]
    fn a_rejected_edit_leaves_the_buffer_untouched() {
        // Default policy: edits need approval. The user rejects, so the buffer is not modified.
        let mut buffer = Buffer::from_text("keep me\n");
        let tag = tag_of(&buffer, 0);
        let provider = MockProvider::new([
            CompletionResponse::calling(
                "",
                vec![tool_call(
                    "edit",
                    json!({ "edits": [{ "line": 1, "tag": tag, "op": "replace", "text": "CLOBBERED" }] }),
                )],
            ),
            CompletionResponse::text("ok, leaving it"),
        ]);
        let mut session =
            AgentSession::new(Box::new(provider), Policy::default(), "You edit code.");
        let mut approver = FixedApprover(false); // user rejects the edit

        let outcome = session.run(&mut buffer, &mut approver, "wreck it");

        assert_eq!(outcome, Outcome::Done("ok, leaving it".to_owned()));
        assert_eq!(
            buffer.text(),
            "keep me\n",
            "a rejected edit must not change the buffer"
        );
    }

    #[test]
    fn an_engaged_kill_switch_stops_the_turn() {
        let mut buffer = Buffer::from_text("x\n");
        let provider = MockProvider::new([CompletionResponse::text("unreached")]);
        let mut session = AgentSession::new(Box::new(provider), Policy::default(), "sys");
        session.kill_switch().engage(); // engage via a clone, as the UI would

        let mut approver = FixedApprover(true);
        let outcome = session.run(&mut buffer, &mut approver, "go");

        assert_eq!(outcome, Outcome::Stopped);
    }
}
