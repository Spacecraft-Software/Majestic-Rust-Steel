// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The editor-side host for the governed Architect agent (behind the default-on `agent` feature).
//!
//! [`AgentHost`] owns the long-lived agent configuration — a shared model [`Provider`], the Seraph
//! [`Policy`], the system prompt, and the buffer tool specs — plus the in-flight turn (an
//! [`AgentRunner`]) and a pending approval awaiting the user. The TUI drives it: a submitted panel
//! message [`Self::start_turn`]s a turn, [`Self::poll`] is called each frame to service the off-thread
//! worker against the active buffer, [`Self::answer_approval`] resolves the y/n prompt, and
//! [`Self::stop`] is the `agent-stop-all` panic button. All of this is gated by Seraph in the worker;
//! the host only marshals buffer access and the approval prompt back to the UI thread.

use std::sync::mpsc::Sender;
use std::sync::Arc;

use architect::{
    Approval, CompletionRequest, HttpProvider, Message, Outcome, Provider, ToolCall, ToolSpec,
    Tools,
};
use majestic_agent::{
    buffer_tool_specs, preview_edits, shell_tool_spec, AgentEvent, AgentRunner, BufferTools,
};
use majestic_config::{AgentConfig, Config};
use majestic_core::Buffer;
use seraph::{KillSwitch, Policy, Sandbox};

use crate::agent_panel::AgentPanel;

/// The agent's system prompt: who it is and how to use the buffer tools (the hashline discipline).
const AGENT_SYSTEM_PROMPT: &str = "You are Majestic's coding agent, embedded in the user's terminal \
editor. You have two tools over the file currently open in the editor: `read` returns it as \
`LINE:TAG│text` lines, and `edit` changes it by citing each line's 1-based LINE and its TAG exactly \
as `read` showed them. Always `read` immediately before you `edit` so your tags are fresh; if an \
edit is rejected for a stale tag, `read` again and retry. Make minimal, correct changes and briefly \
say what you did.";

/// The step ceiling for one agent turn — bounds a misbehaving model's tool-call loop.
const AGENT_MAX_STEPS: usize = 16;

/// Appended to the system prompt when the shell tool is enabled, so the model knows it exists and how
/// it is constrained.
const SHELL_PROMPT_ADDENDUM: &str =
    " You also have a `shell` tool that runs ONE program (no pipes, \
redirects, chaining, or globs) in the project directory and returns its output. Only allow-listed \
programs run and the user approves every command, so prefer it for read-only checks (build, test, \
status) and keep commands simple.";

/// The editor-side owner of the agent's configuration and its in-flight turn.
pub struct AgentHost {
    provider: Arc<dyn Provider>,
    policy: Policy,
    system_prompt: String,
    tools: Vec<ToolSpec>,
    sandbox: Sandbox,
    runner: Option<AgentRunner>,
    pending_approval: Option<(ToolCall, Sender<Approval>)>,
    /// Whether the current turn has streamed any assistant text — so `Finished` does not re-print a
    /// reply the panel already rendered token by token.
    streamed: bool,
}

impl AgentHost {
    /// Builds the host from the Nickel manifest (PRD #1 §5.4): the `agent` section selects the provider
    /// (the API key still from the environment, §9), the `seraph` section is the fail-closed policy
    /// (shell off unless its `shell_allowlist` names programs), and the read/edit tools gain `shell`
    /// when it is enabled. A missing or invalid manifest falls back to the closed defaults.
    #[must_use]
    pub fn new() -> Self {
        let config = load_manifest();
        let policy = config.seraph;
        let shell_enabled = !policy.shell_allowlist.is_empty();

        let mut tools = buffer_tool_specs();
        let mut system_prompt = AGENT_SYSTEM_PROMPT.to_owned();
        if shell_enabled {
            tools.push(shell_tool_spec());
            system_prompt.push_str(SHELL_PROMPT_ADDENDUM);
        }

        // Isolate the shell's network unless the policy explicitly allows network hosts — consistent
        // with the agent's network policy, which is empty (deny) by default.
        let isolate_network = policy.network_allowlist.is_empty();
        // Read-only-root filesystem isolation is opt-in via the manifest (off by default — it breaks
        // tools that write outside the project, e.g. cargo's registry cache).
        let isolate_filesystem = policy.isolate_filesystem;
        Self {
            provider: build_provider(&config.agent),
            policy,
            system_prompt,
            tools,
            sandbox: Sandbox::new(project_root())
                .with_network_isolation(isolate_network)
                .with_filesystem_isolation(isolate_filesystem),
            runner: None,
            pending_approval: None,
            streamed: false,
        }
    }

    /// Whether a turn is in flight (so the frame loop polls more responsively while it runs).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.runner.is_some()
    }

    /// Whether a tool call is awaiting the user's y/n approval.
    #[must_use]
    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Starts a turn for `message`: builds the request (system prompt + tool specs + the user message)
    /// and spawns the off-thread runner. A no-op with a note if a turn is already running.
    pub fn start_turn(&mut self, panel: &mut AgentPanel, message: &str) {
        if self.runner.is_some() {
            panel.push_system("the agent is still working — press Ctrl+Shift+K to stop it");
            return;
        }
        let mut request = CompletionRequest::new()
            .with_message(Message::system(self.system_prompt.clone()))
            .with_message(Message::user(message));
        for tool in &self.tools {
            request = request.with_tool(tool.clone());
        }
        self.streamed = false;
        self.runner = Some(AgentRunner::spawn(
            Arc::clone(&self.provider),
            self.policy.clone(),
            KillSwitch::new(),
            request,
            AGENT_MAX_STEPS,
            self.sandbox.clone(),
        ));
    }

    /// Answers a pending approval (Apply or Reject) and notes the decision; the blocked worker then
    /// continues.
    pub fn answer_approval(&mut self, panel: &mut AgentPanel, approve: bool) {
        if let Some((call, reply)) = self.pending_approval.take() {
            let _ = reply.send(if approve {
                Approval::Run
            } else {
                Approval::Reject
            });
            panel.push_system(if approve {
                format!("approved `{}`", call.name)
            } else {
                format!("rejected `{}`", call.name)
            });
        }
    }

    /// The proposed text of a pending single-edit approval — what the Edit option pre-fills. `None`
    /// when there is no editable pending edit (no approval, not an `edit`, or a multi-edit batch).
    #[must_use]
    pub fn pending_edit_text(&self) -> Option<String> {
        let (call, _) = self.pending_approval.as_ref()?;
        if call.name != "edit" {
            return None;
        }
        let edits = call.arguments.get("edits")?.as_array()?;
        if edits.len() != 1 {
            return None; // editing is offered only for a single proposed edit
        }
        edits[0]
            .get("text")?
            .as_str()
            .map(std::borrow::ToOwned::to_owned)
    }

    /// Applies the pending edit with the user's edited `text` substituted (the Edit option): the
    /// blocked worker runs the modified call instead of the original.
    pub fn answer_modified(&mut self, panel: &mut AgentPanel, text: &str) {
        if let Some((mut call, reply)) = self.pending_approval.take() {
            if let Some(slot) = call.arguments.pointer_mut("/edits/0/text") {
                *slot = serde_json::Value::String(text.to_owned());
            }
            let _ = reply.send(Approval::RunModified(call));
            panel.push_system("applied your edited change");
        }
    }

    /// The `agent-stop-all` panic button: engages the running turn's kill switch and answers any
    /// pending approval as a rejection so a blocked worker unblocks and stops promptly (≤100 ms).
    pub fn stop(&mut self, panel: &mut AgentPanel) {
        if let Some(runner) = self.runner.as_ref() {
            runner.kill_switch().engage();
            panel.push_system("stopping the agent…");
        }
        if let Some((_, reply)) = self.pending_approval.take() {
            let _ = reply.send(Approval::Reject);
        }
    }

    /// Services the agent worker: replies to classify/run requests against the live `buffer`, raises
    /// the approval prompt, and shows the final reply. Non-blocking — drains what is ready.
    pub fn poll(&mut self, panel: &mut AgentPanel, buffer: &mut Buffer) {
        // While an approval is pending the worker is blocked on our answer; nothing new will arrive.
        if self.pending_approval.is_some() {
            return;
        }
        loop {
            // Re-borrow the runner only to take one (owned) event, so the match may mutate `self`.
            let event = match self.runner.as_ref() {
                Some(runner) => runner.poll(),
                None => return,
            };
            let Some(event) = event else {
                return;
            };
            match event {
                AgentEvent::Classify { call, reply } => {
                    let _ = reply.send(BufferTools::new(buffer).action(&call));
                }
                AgentEvent::Execute { call, reply } => {
                    let _ = reply.send(BufferTools::new(buffer).run(&call));
                }
                AgentEvent::Approve { call, reply } => {
                    show_approval_diff(panel, buffer, &call);
                    self.pending_approval = Some((call, reply));
                    return; // the worker is blocked until we answer
                }
                AgentEvent::Token(chunk) => {
                    self.streamed = true;
                    panel.stream_token(&chunk);
                }
                AgentEvent::Finished { outcome, .. } => {
                    panel.end_stream();
                    // A `Done` reply was already rendered live via tokens — don't print it twice;
                    // anything else (stopped / step-limit / error) gets its status note.
                    if !(self.streamed && matches!(outcome, Outcome::Done(_))) {
                        panel.push_agent(outcome_text(&outcome));
                    }
                    self.runner = None;
                    return;
                }
            }
        }
    }
}

/// Loads the Nickel manifest (the same one the editor's settings come from), falling back to the
/// closed defaults if it is absent or invalid — the host surfaces a separate notice for an invalid
/// manifest, so a silent default here is acceptable.
fn load_manifest() -> Config {
    Config::discover()
        .and_then(|path| Config::load(&path).ok())
        .unwrap_or_default()
}

/// Builds the agent's model provider from the manifest's `agent` section, local-first (Ollama by
/// default). The bearer key is read from `MAJESTIC_AGENT_KEY` in the environment — never the manifest
/// (PRD #1 §9).
fn build_provider(agent: &AgentConfig) -> Arc<dyn Provider> {
    let key = std::env::var("MAJESTIC_AGENT_KEY").ok();
    Arc::new(HttpProvider::new(
        agent.base_url.clone(),
        agent.model.clone(),
        key,
    ))
}

/// The project root the sandbox runs approved commands in — the working directory.
fn project_root() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Shows the approval prompt for `call`: the exact command for a `shell` call, a +/- diff for an
/// `edit`, or a plain confirmation otherwise.
fn show_approval_diff(panel: &mut AgentPanel, buffer: &Buffer, call: &ToolCall) {
    if call.name == "shell" {
        panel.push_system("run this command? (y = run / n = reject)");
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<missing command>");
        panel.push_system(format!("$ {command}"));
        return;
    }
    let preview = preview_edits(buffer, call);
    if preview.is_empty() {
        panel.push_system(format!("approve `{}`? (y = yes / n = no)", call.name));
        return;
    }
    panel.push_system("apply this edit? (y = apply / e = edit / n = reject)");
    for change in preview {
        panel.push_system(format!("@ line {}", change.line));
        if let Some(old) = change.old {
            panel.push_diff_removed(old);
        }
        if let Some(new) = change.new {
            panel.push_diff_added(new);
        }
    }
}

/// The transcript text shown for a finished turn's [`Outcome`].
fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done(text) if text.is_empty() => "(done)".to_owned(),
        Outcome::Done(text) => text.clone(),
        Outcome::Stopped => "(stopped)".to_owned(),
        Outcome::BudgetExhausted => "(reached the step limit without finishing)".to_owned(),
        Outcome::Failed(error) => format!("(agent error: {error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::{outcome_text, AgentHost};
    use architect::{Outcome, ProviderError};

    #[test]
    fn outcome_text_maps_each_outcome() {
        assert_eq!(outcome_text(&Outcome::Done("hi".to_owned())), "hi");
        assert_eq!(outcome_text(&Outcome::Done(String::new())), "(done)");
        assert_eq!(outcome_text(&Outcome::Stopped), "(stopped)");
        assert!(outcome_text(&Outcome::BudgetExhausted).contains("step limit"));
        assert!(
            outcome_text(&Outcome::Failed(ProviderError::Backend("boom".to_owned())))
                .contains("boom")
        );
    }

    #[test]
    fn a_fresh_host_is_idle() {
        let host = AgentHost::new();
        assert!(!host.is_running());
        assert!(!host.has_pending_approval());
    }
}
