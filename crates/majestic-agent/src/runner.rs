// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`AgentRunner`] — the off-thread driver that keeps the editor responsive while the agent runs.
//!
//! [`AgentSession::run`](crate::AgentSession) is synchronous: it would block the UI thread for the
//! whole turn — every (possibly slow) model call and every tool. The renderer can't spin at 60 Hz
//! through that, and an interactive approval can't pause a synchronous call to ask the user. So the
//! turn runs on a **worker thread** (the pattern `provider.rs` and `majestic-lsp` prescribe: work on a
//! thread, results over a channel, poll each frame), and the two things that *must* happen on the main
//! thread — touching the buffer and prompting the user — are **marshaled back** over channels:
//!
//! - The worker drives [`architect::run_turn`] with a [`ChannelTools`] and a [`ChannelApprover`].
//! - Each time the loop classifies a call, runs a call, or needs approval, the worker sends an
//!   [`AgentEvent`] carrying a one-shot reply channel and **blocks** until the host answers.
//! - The host [`AgentRunner::poll`]s each frame, services [`AgentEvent::Classify`] /
//!   [`AgentEvent::Execute`] against the live buffer (via [`BufferTools`](crate::BufferTools)) and
//!   stashes [`AgentEvent::Approve`] for its dialog, then replies. The worker unblocks and continues.
//! - When the loop ends, the worker sends [`AgentEvent::Finished`] with the [`Outcome`] and the turn's
//!   audit log.
//!
//! Classification needs the live buffer too ([`BufferTools::action`](crate::BufferTools) reads the
//! buffer's path), so it is marshaled like execution rather than guessed on the worker — one source of
//! truth, and the round-trip is a cheap in-process channel hop the (already-blocked) worker pays.
//!
//! Stopping is two-layered, matching [`KillSwitch`]'s contract: engage the switch (cooperative; the
//! loop checks it every step) — which [`AgentRunner`] also does on drop, so dropping a runner aborts
//! its turn. The worker is **detached, never joined**, so the UI thread never blocks on a slow model
//! call; a dropped runner's receiver makes the worker's next send fail, unblocking any reply it awaits.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use architect::{
    run_turn, Approver, CompletionRequest, Governor, Outcome, Provider, ToolCall, Tools,
};
use seraph::{AgentAction, AuditLog, KillSwitch, Policy};

/// Something the worker needs the host to do, or the turn's result. Every request variant carries a
/// one-shot [`Sender`] the host answers on; the worker blocks until that reply arrives.
#[derive(Debug)]
pub enum AgentEvent {
    /// Classify `call`'s Seraph action (needs the live buffer). Answer with the [`AgentAction`].
    Classify {
        /// The tool call to classify.
        call: ToolCall,
        /// The channel to send the classification back on.
        reply: Sender<AgentAction>,
    },
    /// Run an already-gated `call` against the buffer. Answer with its output or an error message.
    Execute {
        /// The approved tool call to run.
        call: ToolCall,
        /// The channel to send the run result back on.
        reply: Sender<Result<String, String>>,
    },
    /// Ask the user whether to run `call` (the diff/confirm dialog). Answer `true` to approve.
    Approve {
        /// The tool call awaiting the user's decision.
        call: ToolCall,
        /// The channel to send the user's yes/no back on.
        reply: Sender<bool>,
    },
    /// The turn ended. Carries how it ended and the audit log of every gated decision in it.
    Finished {
        /// How the turn ended.
        outcome: Outcome,
        /// The audit log accumulated during the turn.
        audit: AuditLog,
    },
}

/// A handle to one in-flight agent turn running on a worker thread. The host [`Self::poll`]s it each
/// frame to service the worker's requests and observe completion. Dropping it stops the turn.
#[derive(Debug)]
pub struct AgentRunner {
    events: Receiver<AgentEvent>,
    kill: KillSwitch,
}

impl AgentRunner {
    /// Spawns a worker thread that runs one governed turn over `request` (up to `max_steps` model
    /// rounds), gating every tool call through `policy` and honoring `kill`. The provider is shared
    /// (`Arc`) so the host can reuse it across turns. Returns immediately; drive it with [`Self::poll`].
    ///
    /// # Panics
    /// Panics only if the OS refuses to spawn the worker thread (e.g. the process is out of threads),
    /// which an interactive editor cannot meaningfully recover from.
    #[must_use]
    pub fn spawn(
        provider: Arc<dyn Provider>,
        policy: Policy,
        kill: KillSwitch,
        request: CompletionRequest,
        max_steps: usize,
    ) -> Self {
        let (events, rx) = mpsc::channel();
        let worker_kill = kill.clone();
        thread::Builder::new()
            .name("architect-agent".to_owned())
            .spawn(move || {
                let mut tools = ChannelTools {
                    events: events.clone(),
                };
                let mut approver = ChannelApprover {
                    events: events.clone(),
                };
                let mut audit = AuditLog::new();
                let outcome = {
                    let mut governor = Governor::new(&policy, &mut audit, &worker_kill);
                    run_turn(
                        provider.as_ref(),
                        &mut tools,
                        &mut approver,
                        &mut governor,
                        jiff::Timestamp::now,
                        request,
                        max_steps,
                    )
                };
                // Best-effort: if the host dropped the runner, the receiver is gone and this just fails.
                let _ = events.send(AgentEvent::Finished { outcome, audit });
            })
            .expect("OS refused to spawn the agent worker thread");
        Self { events: rx, kill }
    }

    /// The next thing the worker needs the host to do, or `None` if nothing is pending yet (and the
    /// worker is still running). Non-blocking — call it each frame in a `while let Some(_)` loop.
    ///
    /// For a request variant, the host must answer on its `reply` channel; for
    /// [`AgentEvent::Finished`] the turn is over and the runner may be dropped.
    ///
    /// `None` covers both an empty channel (still running) and a disconnected one (the worker exited
    /// without a `Finished` — e.g. its send raced our drop); either way there is nothing to service.
    #[must_use]
    pub fn poll(&self) -> Option<AgentEvent> {
        self.events.try_recv().ok()
    }

    /// A clone of this turn's kill switch — engage it (e.g. from `agent-stop-all`) to stop the agent.
    #[must_use]
    pub fn kill_switch(&self) -> KillSwitch {
        self.kill.clone()
    }
}

impl Drop for AgentRunner {
    fn drop(&mut self) {
        // Stop the worker promptly without blocking the UI: engage the kill switch (the loop checks it
        // every step) and let the thread detach. Dropping `events` (the receiver) makes the worker's
        // next channel send fail, unblocking any reply it is currently waiting on.
        self.kill.engage();
    }
}

/// The worker-side [`Tools`]: every classification and run is marshaled to the host over `events`.
struct ChannelTools {
    events: Sender<AgentEvent>,
}

impl Tools for ChannelTools {
    fn action(&self, call: &ToolCall) -> AgentAction {
        let (reply, rx) = mpsc::channel();
        let request = AgentEvent::Classify {
            call: call.clone(),
            reply,
        };
        // If the host is gone, classify as a path read: harmless on its own, and the matching
        // `run` will fail closed, after which the kill switch (engaged on the host's drop) stops us.
        if self.events.send(request).is_err() {
            return AgentAction::ReadPath {
                path: std::path::PathBuf::new(),
            };
        }
        rx.recv().unwrap_or(AgentAction::ReadPath {
            path: std::path::PathBuf::new(),
        })
    }

    fn run(&mut self, call: &ToolCall) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        let request = AgentEvent::Execute {
            call: call.clone(),
            reply,
        };
        if self.events.send(request).is_err() {
            return Err(host_gone());
        }
        match rx.recv() {
            Ok(result) => result,
            Err(_disconnected) => Err(host_gone()),
        }
    }
}

/// The worker-side [`Approver`]: the prompt is marshaled to the host's dialog over `events`.
struct ChannelApprover {
    events: Sender<AgentEvent>,
}

impl Approver for ChannelApprover {
    fn approve(&mut self, call: &ToolCall) -> bool {
        let (reply, rx) = mpsc::channel();
        let request = AgentEvent::Approve {
            call: call.clone(),
            reply,
        };
        // Fail closed: a missing host means no approval, so a guarded action never runs unattended.
        if self.events.send(request).is_err() {
            return false;
        }
        rx.recv().unwrap_or(false)
    }
}

/// The error fed back to the model (and surfaced) when the editor stops servicing the worker.
fn host_gone() -> String {
    "the editor stopped servicing the agent".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, AgentRunner};
    use crate::{buffer_tool_specs, BufferTools};
    use architect::{
        CompletionRequest, CompletionResponse, Message, MockProvider, Outcome, ToolCall, Tools,
    };
    use majestic_core::{tagged_read, Buffer};
    use seraph::{KillSwitch, Policy};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn tool_call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// The tag `tagged_read` assigned to 0-based `line` of `buffer` (as the model would cite it).
    fn tag_of(buffer: &Buffer, line: usize) -> String {
        let read = tagged_read(buffer);
        let row = read.lines().nth(line).expect("line present");
        let after_colon = row.split_once(':').expect("colon").1;
        after_colon.split_once('│').expect("separator").0.to_owned()
    }

    fn request(user: &str) -> CompletionRequest {
        let mut request = CompletionRequest::new().with_message(Message::user(user));
        for spec in buffer_tool_specs() {
            request = request.with_tool(spec);
        }
        request
    }

    /// Drives a runner to completion exactly as the editor's frame loop will: poll for events, service
    /// `Classify`/`Execute` against the real `buffer`, answer every `Approve` with `approve`. Returns
    /// the turn's outcome. Busy-polls (with a yield) — fine for a test against a [`MockProvider`].
    fn drive(runner: &AgentRunner, buffer: &mut Buffer, approve: bool) -> Outcome {
        loop {
            match runner.poll() {
                Some(AgentEvent::Classify { call, reply }) => {
                    let action = BufferTools::new(buffer).action(&call);
                    let _ = reply.send(action);
                }
                Some(AgentEvent::Execute { call, reply }) => {
                    let result = BufferTools::new(buffer).run(&call);
                    let _ = reply.send(result);
                }
                Some(AgentEvent::Approve { reply, .. }) => {
                    let _ = reply.send(approve);
                }
                Some(AgentEvent::Finished { outcome, .. }) => return outcome,
                None => thread::yield_now(),
            }
        }
    }

    #[test]
    fn a_full_turn_edits_the_real_buffer_off_thread() {
        // The off-thread capstone: a scripted model reads then edits a REAL buffer entirely through
        // the worker + channel marshaling, with the host servicing every request as the UI would.
        let mut buffer = Buffer::from_text("fn main() {}\n");
        let tag = tag_of(&buffer, 0);
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
        let policy = Policy {
            edits_need_approval: false, // auto-approve, so no dialog is needed this turn
            ..Policy::default()
        };
        let runner = AgentRunner::spawn(
            Arc::new(provider),
            policy,
            KillSwitch::new(),
            request("add a hello print"),
            8,
        );

        let outcome = drive(&runner, &mut buffer, false);

        assert_eq!(outcome, Outcome::Done("done".to_owned()));
        assert!(
            buffer.text().contains("println!"),
            "the buffer was edited off-thread through the governed stack: {}",
            buffer.text()
        );
    }

    #[test]
    fn an_approved_edit_runs_and_a_rejected_one_does_not() {
        for (approve, expected) in [(true, "EDITED\n"), (false, "keep me\n")] {
            let mut buffer = Buffer::from_text("keep me\n");
            let tag = tag_of(&buffer, 0);
            let provider = MockProvider::new([
                CompletionResponse::calling(
                    "",
                    vec![tool_call(
                        "edit",
                        json!({ "edits": [{ "line": 1, "tag": tag, "op": "replace", "text": "EDITED" }] }),
                    )],
                ),
                CompletionResponse::text("ok"),
            ]);
            // Default policy: edits need approval, so the worker raises an Approve event.
            let runner = AgentRunner::spawn(
                Arc::new(provider),
                Policy::default(),
                KillSwitch::new(),
                request("edit it"),
                8,
            );

            let outcome = drive(&runner, &mut buffer, approve);

            assert_eq!(outcome, Outcome::Done("ok".to_owned()));
            assert_eq!(
                buffer.text(),
                expected,
                "approve={approve} should leave the buffer as {expected:?}"
            );
        }
    }

    #[test]
    fn an_engaged_kill_switch_stops_the_turn() {
        let mut buffer = Buffer::from_text("x\n");
        let provider = MockProvider::new([CompletionResponse::text("unreached")]);
        let kill = KillSwitch::new();
        kill.engage(); // engaged before the first step, as the UI's stop key would
        let runner = AgentRunner::spawn(
            Arc::new(provider),
            Policy::default(),
            kill,
            request("go"),
            8,
        );

        assert_eq!(drive(&runner, &mut buffer, true), Outcome::Stopped);
    }

    /// M3 exit criterion (PRD §5.2.4): agent-stop-all halts the loop within ≤100 ms of engaging. A
    /// provider that always asks for another read would run to the (huge) step budget; we let a few
    /// rounds run, engage the kill switch, and measure the time from engage to the worker reporting
    /// `Stopped`. The cooperative check sits at every step, so this is microseconds in practice.
    #[test]
    fn m3_exit_engaging_the_kill_switch_stops_within_the_budget() {
        let provider = MockProvider::new(
            std::iter::repeat_with(|| {
                CompletionResponse::calling("", vec![tool_call("read", json!({}))])
            })
            .take(10_000),
        );
        let kill = KillSwitch::new();
        let runner = AgentRunner::spawn(
            Arc::new(provider),
            Policy::default(),
            kill.clone(),
            request("loop"),
            100_000,
        );

        let mut buffer = Buffer::from_text("x\n");
        let mut serviced = 0u32;
        let mut engaged_at: Option<Instant> = None;
        let outcome = loop {
            match runner.poll() {
                Some(AgentEvent::Classify { call, reply }) => {
                    let _ = reply.send(BufferTools::new(&mut buffer).action(&call));
                }
                Some(AgentEvent::Execute { call, reply }) => {
                    let _ = reply.send(BufferTools::new(&mut buffer).run(&call));
                    serviced += 1;
                    if serviced == 3 {
                        engaged_at = Some(Instant::now());
                        kill.engage(); // the user's agent-stop-all, mid-run
                    }
                }
                Some(AgentEvent::Approve { reply, .. }) => {
                    let _ = reply.send(true);
                }
                Some(AgentEvent::Finished { outcome, .. }) => break outcome,
                None => thread::yield_now(),
            }
        };

        assert_eq!(outcome, Outcome::Stopped);
        let elapsed = engaged_at.expect("the kill switch was engaged").elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "agent-stop-all must halt within 100 ms; took {elapsed:?}"
        );
    }
}
