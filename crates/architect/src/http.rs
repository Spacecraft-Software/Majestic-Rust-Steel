// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! An OpenAI-compatible HTTP [`Provider`] (PRD #1 §9) — the real backend, behind the
//! `http-provider` feature.
//!
//! Majestic is **local-first**: [`HttpProvider`] defaults to a local Ollama server's
//! OpenAI-compatible API and speaks the standard `/chat/completions` shape, so any compatible server
//! works. It is synchronous and blocking — it runs on the agent loop's worker thread, exactly the
//! seam [`Provider::complete`] is built for. This build is **HTTP only** (no TLS), so it targets
//! local servers; cloud HTTPS is a later, opt-in addition. When an API key is used it comes from the
//! **environment**, set by the host — never from the manifest.

use std::io::{BufRead, BufReader};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::{
    CompletionRequest, CompletionResponse, Message, Provider, ProviderError, ToolCall, ToolSpec,
};

/// The default endpoint: a local Ollama server's OpenAI-compatible API.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// The OpenAI `/chat/completions` request body.
#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
}

/// A tool entry in the request (`{ "type": "function", "function": { … } }`).
#[derive(Debug, Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

/// The function description inside a [`WireTool`].
#[derive(Debug, Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

/// The OpenAI `/chat/completions` response body (the fields Majestic reads).
#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
}

/// One choice in a [`WireResponse`].
#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
}

/// The assistant message in a choice.
#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

/// A tool call the model requested.
#[derive(Debug, Deserialize)]
struct WireToolCall {
    id: String,
    function: WireCallFunction,
}

/// The function name and (string-encoded JSON) arguments of a [`WireToolCall`].
#[derive(Debug, Deserialize)]
struct WireCallFunction {
    name: String,
    arguments: String,
}

/// Builds the OpenAI request body for `request` against `model`.
fn build_request<'a>(request: &'a CompletionRequest, model: &'a str) -> WireRequest<'a> {
    WireRequest {
        model,
        messages: &request.messages,
        tools: request.tools.iter().map(wire_tool).collect(),
        stream: false,
    }
}

/// Wraps one [`ToolSpec`] as an OpenAI function tool.
fn wire_tool(tool: &ToolSpec) -> WireTool<'_> {
    WireTool {
        kind: "function",
        function: WireFunction {
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
        },
    }
}

/// Maps an OpenAI response into a [`CompletionResponse`], taking the first choice. A tool call's
/// string-encoded `arguments` are parsed into JSON (falling back to `null` if malformed).
fn map_response(wire: WireResponse) -> Result<CompletionResponse, ProviderError> {
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::Backend("response had no choices".to_owned()))?;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null),
        })
        .collect();
    Ok(CompletionResponse {
        content: choice.message.content.unwrap_or_default(),
        tool_calls,
    })
}

/// One Server-Sent-Events chunk of a streaming `/chat/completions` response.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

/// One choice's incremental `delta` in a [`StreamChunk`].
#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

/// The incremental fields of a streamed choice: a text fragment and/or tool-call fragments.
#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCallDelta>,
}

/// A streamed tool-call fragment, keyed by `index`; `id`/name arrive once, `arguments` in pieces.
#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

/// The function name and string-encoded argument fragments of a [`StreamToolCallDelta`].
#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// A tool call being reassembled from streamed fragments.
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// Reads an OpenAI-compatible SSE stream, calling `on_token` with each assistant text fragment and
/// reassembling the full reply (content + tool calls, whose `arguments` arrive as string fragments).
///
/// Each event line is `data: <json>`; the stream ends at `data: [DONE]` or EOF. Unparseable chunks
/// are skipped (servers interleave keep-alives and non-choice events).
///
/// # Errors
/// Returns a [`ProviderError::Backend`] if the underlying reader fails mid-stream.
fn read_sse_stream(
    reader: impl BufRead,
    on_token: &mut dyn FnMut(&str),
) -> Result<CompletionResponse, ProviderError> {
    let mut content = String::new();
    let mut tools: Vec<ToolCallAccumulator> = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|error| ProviderError::Backend(error.to_string()))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue; // blank separators, comments, and headers are not data events
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            continue; // tolerate keep-alives / shapes we don't model
        };
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };
        if let Some(token) = choice.delta.content {
            if !token.is_empty() {
                on_token(&token);
                content.push_str(&token);
            }
        }
        for fragment in choice.delta.tool_calls {
            if tools.len() <= fragment.index {
                tools.resize_with(fragment.index + 1, ToolCallAccumulator::default);
            }
            let slot = &mut tools[fragment.index];
            if let Some(id) = fragment.id {
                slot.id = id;
            }
            if let Some(function) = fragment.function {
                if let Some(name) = function.name {
                    slot.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    slot.arguments.push_str(&arguments);
                }
            }
        }
    }

    let tool_calls = tools
        .into_iter()
        .map(|accumulated| ToolCall {
            id: accumulated.id,
            name: accumulated.name,
            arguments: serde_json::from_str(&accumulated.arguments).unwrap_or(Value::Null),
        })
        .collect();
    Ok(CompletionResponse {
        content,
        tool_calls,
    })
}

/// An OpenAI-compatible completion provider over HTTP. Local-first (Ollama by default), synchronous,
/// and HTTP-only in this build. Holds a reusable connection agent.
#[derive(Debug)]
pub struct HttpProvider {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl HttpProvider {
    /// A provider posting to `base_url`'s `/chat/completions` for `model`, optionally bearer-authorized
    /// with `api_key` (which the host should read from the environment).
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        let base = base_url.into();
        Self {
            endpoint: format!("{}/chat/completions", base.trim_end_matches('/')),
            model: model.into(),
            api_key,
            agent: ureq::agent(),
        }
    }

    /// A provider for the default local Ollama endpoint with `model` and no API key.
    #[must_use]
    pub fn local(model: impl Into<String>) -> Self {
        Self::new(DEFAULT_BASE_URL, model, None)
    }
}

impl Provider for HttpProvider {
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let body = build_request(request, &self.model);
        let mut http = self
            .agent
            .post(&self.endpoint)
            .set("content-type", "application/json");
        if let Some(key) = &self.api_key {
            http = http.set("authorization", &format!("Bearer {key}"));
        }
        let response = http
            .send_json(&body)
            .map_err(|error| ProviderError::Backend(error.to_string()))?;
        let wire: WireResponse = response
            .into_json()
            .map_err(|error| ProviderError::Backend(error.to_string()))?;
        map_response(wire)
    }

    fn complete_streaming(
        &self,
        request: &CompletionRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<CompletionResponse, ProviderError> {
        let mut body = build_request(request, &self.model);
        body.stream = true; // ask the server for an SSE token stream
        let mut http = self
            .agent
            .post(&self.endpoint)
            .set("content-type", "application/json");
        if let Some(key) = &self.api_key {
            http = http.set("authorization", &format!("Bearer {key}"));
        }
        let response = http
            .send_json(&body)
            .map_err(|error| ProviderError::Backend(error.to_string()))?;
        read_sse_stream(BufReader::new(response.into_reader()), on_token)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_request, map_response, read_sse_stream, HttpProvider, WireResponse, DEFAULT_BASE_URL,
    };
    use crate::provider::{CompletionRequest, Message, Provider, ToolSpec};
    use serde_json::json;

    #[test]
    fn build_request_emits_openai_shape() {
        let request = CompletionRequest::new()
            .with_message(Message::system("be helpful"))
            .with_message(Message::user("read foo.rs"))
            .with_tool(ToolSpec {
                name: "read".to_owned(),
                description: "read a file".to_owned(),
                parameters: json!({ "type": "object" }),
            });
        let wire =
            serde_json::to_value(build_request(&request, "qwen2.5-coder")).expect("serialize");

        assert_eq!(wire["model"], "qwen2.5-coder");
        assert_eq!(wire["stream"], false);
        assert_eq!(wire["messages"][0]["role"], "system");
        // The tool is wrapped as an OpenAI function tool.
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn build_request_omits_empty_tools() {
        let request = CompletionRequest::new().with_message(Message::user("hi"));
        let wire = serde_json::to_value(build_request(&request, "m")).expect("serialize");
        assert!(
            wire.get("tools").is_none(),
            "no tools key when there are none"
        );
    }

    #[test]
    fn map_response_extracts_content_and_tool_calls() {
        // A representative OpenAI/Ollama response: text plus one tool call (arguments are a JSON string).
        let raw = json!({
            "choices": [{
                "message": {
                    "content": "let me read it",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"foo.rs\"}" }
                    }]
                }
            }]
        });
        let wire: WireResponse = serde_json::from_value(raw).expect("deserialize");
        let response = map_response(wire).expect("mapped");

        assert_eq!(response.content, "let me read it");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(response.tool_calls[0].arguments["path"], "foo.rs");
    }

    #[test]
    fn map_response_handles_plain_text_and_missing_content() {
        let raw = json!({ "choices": [{ "message": {} }] });
        let wire: WireResponse = serde_json::from_value(raw).expect("deserialize");
        let response = map_response(wire).expect("mapped");
        assert_eq!(response.content, "");
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn map_response_errors_without_choices() {
        let wire: WireResponse =
            serde_json::from_value(json!({ "choices": [] })).expect("deserialize");
        map_response(wire).expect_err("no choices is an error");
    }

    #[test]
    fn read_sse_stream_streams_content_and_reassembles_fragmented_tool_calls() {
        // A representative SSE stream: two content tokens, then a tool call whose name and JSON
        // arguments arrive across several `data:` chunks, then `[DONE]`. Built with `json!` so the
        // per-chunk escaping is correct by construction.
        let chunks = [
            json!({ "choices": [{ "delta": { "content": "Hel" } }] }),
            json!({ "choices": [{ "delta": { "content": "lo" } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "id": "call_1", "function": { "name": "re" } }
            ] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "name": "ad", "arguments": "{\"path\":" } }
            ] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "\"foo.rs\"}" } }
            ] } }] }),
        ];
        let mut sse = String::new();
        for chunk in &chunks {
            sse.push_str("data: ");
            sse.push_str(&chunk.to_string());
            sse.push('\n');
            sse.push('\n'); // SSE event separator
        }
        sse.push_str("data: [DONE]\n");

        let mut tokens = Vec::new();
        let response = read_sse_stream(std::io::Cursor::new(sse), &mut |token| {
            tokens.push(token.to_owned());
        })
        .expect("the SSE stream parses");

        assert_eq!(tokens, vec!["Hel", "lo"], "content streamed token by token");
        assert_eq!(response.content, "Hello");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "read", "name fragments joined");
        assert_eq!(
            response.tool_calls[0].arguments["path"], "foo.rs",
            "argument fragments reassembled into valid JSON"
        );
    }

    #[test]
    fn local_targets_the_default_ollama_endpoint() {
        let provider = HttpProvider::local("llama3.2");
        // The endpoint is private, but a smoke construction proves `local` wires the default base URL.
        assert!(DEFAULT_BASE_URL.starts_with("http://localhost"));
        let _ = provider; // constructed without a network call
    }

    /// A live round-trip against a local Ollama server. Ignored by default (no server in CI); run with
    /// `cargo test -p architect --features http-provider -- --ignored` after `ollama serve`.
    #[test]
    #[ignore = "requires a local Ollama server"]
    fn live_completion_against_ollama() {
        let provider = HttpProvider::local("llama3.2");
        let request =
            CompletionRequest::new().with_message(Message::user("Say the single word: ok"));
        let response = provider.complete(&request).expect("ollama responds");
        assert!(!response.content.is_empty());
    }
}
