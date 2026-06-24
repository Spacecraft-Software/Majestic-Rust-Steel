// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The model-provider abstraction — the seam every LLM backend plugs into (PRD #1 §5.2.3, §9).
//!
//! Architect is provider-agnostic and local-first: the agent loop talks to a [`Provider`], never to
//! a concrete backend, so Ollama, an OpenAI-compatible server, or an in-process model are all just
//! implementations of one trait. [`Provider::complete`] is **synchronous** on purpose: the loop runs
//! it on a worker thread (the same off-thread pattern `majestic-lsp` uses — request on a thread,
//! deliver the result over a channel, poll each frame), which keeps providers simple and leaves
//! cancellation to the kill switch plus dropping the worker's task.
//!
//! The message, tool, and request/response types mirror the OpenAI-compatible wire shape (lowercase
//! roles, JSON-Schema tool parameters, JSON tool-call arguments) so a real backend serializes
//! straight from them. [`MockProvider`] scripts responses for tests and offline runs.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who authored a [`Message`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The system prompt establishing the agent's instructions and constraints.
    System,
    /// Input from the user.
    User,
    /// A reply from the model.
    Assistant,
    /// The result of a tool the model called, fed back into the conversation.
    Tool,
}

/// One turn in the conversation sent to the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who authored this turn.
    pub role: Role,
    /// The text of the turn (a tool result for [`Role::Tool`]).
    pub content: String,
    /// For a [`Role::Tool`] message, the id of the [`ToolCall`] it answers; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A message with `role` and `content` and no tool-call id.
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
        }
    }

    /// A system-prompt message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// A user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// An assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    /// A tool-result message answering the [`ToolCall`] with id `call_id`.
    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// A tool the model may call, described for function-calling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool's name (what the model puts in a [`ToolCall`]).
    pub name: String,
    /// A natural-language description of what the tool does and when to use it.
    pub description: String,
    /// JSON Schema describing the tool's argument object.
    pub parameters: Value,
}

/// A tool invocation the model requested.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The id the result message references (see [`Message::tool`]).
    pub id: String,
    /// The name of the tool to run.
    pub name: String,
    /// The arguments, as a JSON object matching the tool's [`ToolSpec::parameters`].
    pub arguments: Value,
}

/// What the agent loop sends the model: the conversation so far and the tools it may call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The conversation, oldest first.
    pub messages: Vec<Message>,
    /// The tools available to the model this turn.
    pub tools: Vec<ToolSpec>,
}

impl CompletionRequest {
    /// An empty request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `message`, returning `self` for chaining.
    #[must_use]
    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Appends `tool`, returning `self` for chaining.
    #[must_use]
    pub fn with_tool(mut self, tool: ToolSpec) -> Self {
        self.tools.push(tool);
        self
    }
}

/// The model's reply: assistant text and any tool calls it wants run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The assistant's text (may be empty when the model only requests tool calls).
    pub content: String,
    /// The tool calls the model wants run before it continues (empty when it is done).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl CompletionResponse {
    /// A plain text reply with no tool calls (the model is done).
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }

    /// A reply that requests `tool_calls` (optionally alongside `content`).
    #[must_use]
    pub fn calling(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            content: content.into(),
            tool_calls,
        }
    }

    /// Whether the model requested any tool calls (i.e. the loop must continue).
    #[must_use]
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Why a completion failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// The provider has no more output (e.g. a [`MockProvider`] ran out of scripted responses).
    Exhausted,
    /// The backend reported an error.
    Backend(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => f.write_str("provider produced no further completions"),
            Self::Backend(message) => write!(f, "provider backend error: {message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// A source of model completions — the seam every backend plugs into. `complete` is synchronous so
/// the agent loop can run it on a worker thread; implementations must be `Send + Sync` so that thread
/// can own a shared handle.
pub trait Provider: Send + Sync {
    /// Produces the model's next reply to `request`.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] if the backend fails or has no further output.
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;
}

/// A scripted [`Provider`] for tests and offline runs: it hands back queued responses in order and
/// records every request it received so tests can assert on the conversation the loop built.
#[derive(Debug, Default)]
pub struct MockProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl MockProvider {
    /// A mock that returns `responses` in order, one per [`Provider::complete`] call.
    #[must_use]
    pub fn new(responses: impl IntoIterator<Item = CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The requests this mock has received, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Provider for MockProvider {
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or(ProviderError::Exhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionRequest, CompletionResponse, Message, MockProvider, Provider, ProviderError,
        Role, ToolCall, ToolSpec,
    };
    use serde_json::json;

    #[test]
    fn message_constructors_set_role_and_tool_id() {
        assert_eq!(Message::system("hi").role, Role::System);
        assert_eq!(Message::user("hi").tool_call_id, None);
        let result = Message::tool("call-1", "ok");
        assert_eq!(result.role, Role::Tool);
        assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn request_builder_collects_messages_and_tools() {
        let tool = ToolSpec {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            parameters: json!({ "type": "object" }),
        };
        let request = CompletionRequest::new()
            .with_message(Message::system("be helpful"))
            .with_message(Message::user("read foo.rs"))
            .with_tool(tool);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "read");
    }

    #[test]
    fn response_distinguishes_text_from_tool_calls() {
        assert!(!CompletionResponse::text("done").wants_tools());
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({ "path": "foo.rs" }),
        };
        let response = CompletionResponse::calling("", vec![call]);
        assert!(response.wants_tools());
        assert_eq!(response.tool_calls[0].name, "read");
    }

    #[test]
    fn mock_returns_scripted_responses_in_order_then_exhausts() {
        let provider = MockProvider::new([
            CompletionResponse::text("first"),
            CompletionResponse::text("second"),
        ]);
        let request = CompletionRequest::new().with_message(Message::user("go"));
        assert_eq!(provider.complete(&request).unwrap().content, "first");
        assert_eq!(provider.complete(&request).unwrap().content, "second");
        assert_eq!(provider.complete(&request), Err(ProviderError::Exhausted));
        // It recorded every request it saw.
        assert_eq!(provider.requests().len(), 3);
        assert_eq!(provider.requests()[0].messages[0].content, "go");
    }

    #[test]
    fn request_round_trips_through_json_in_openai_shape() {
        let request = CompletionRequest::new()
            .with_message(Message::system("sys"))
            .with_message(Message::tool("c1", "result"));
        let wire = serde_json::to_string(&request).expect("serialize");
        // Roles serialize lowercase; the tool message carries its call id.
        assert!(wire.contains("\"role\":\"system\""));
        assert!(wire.contains("\"role\":\"tool\""));
        assert!(wire.contains("\"tool_call_id\":\"c1\""));
        let back: CompletionRequest = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, request);
    }
}
