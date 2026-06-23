// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`LspManager`]: spawn/reuse a language server per language, sync open documents, and route
//! published diagnostics back to the editor as byte-range [`Diagnostic`]s (PRD #1 §6.9).
//!
//! One server runs per `languageId` (shared by every open document of that language). The host
//! calls [`LspManager::open`] when a file is shown, [`LspManager::change`] on each edit, and
//! [`LspManager::poll`] each frame to collect diagnostics — converting the server's
//! line/character positions to byte offsets against the document's current text.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lsp_types::{
    CompletionList, CompletionParams, CompletionResponse, Diagnostic as LspDiagnostic,
    DiagnosticSeverity, DocumentFormattingParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, Hover as LspHover, HoverContents, HoverParams, MarkedString,
    MarkupContent, MarkupKind, PartialResultParams, Position, PublishDiagnosticsParams,
    TextDocumentIdentifier, TextDocumentPositionParams, TextEdit, Uri, WorkDoneProgressParams,
};
use majestic_core::{CompletionItem, Diagnostic, Severity};

use crate::client::{file_uri, LanguageServer};
use crate::connection::Requester;

/// How to launch a language server for a file extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// The program to spawn (e.g. `rust-analyzer`).
    pub command: String,
    /// Arguments passed to it.
    pub args: Vec<String>,
    /// The LSP `languageId` (e.g. `rust`).
    pub language_id: String,
}

impl ServerConfig {
    /// A no-argument server `command` speaking `language_id`.
    #[must_use]
    pub fn new(command: impl Into<String>, language_id: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            language_id: language_id.into(),
        }
    }
}

/// The open state of one document (for `didChange` versioning and position conversion).
#[derive(Debug)]
struct DocState {
    language_id: String,
    version: i32,
    text: String,
    /// The `file://` URI sent to the server — used to match its `publishDiagnostics` back to this
    /// document (the URI is canonicalized, so it may differ from the editor's path).
    uri: Uri,
}

/// A language server's lifecycle state. Startup (spawn + the blocking `initialize` handshake) runs
/// on a background thread so opening a file never freezes the editor; the server is used only once
/// it is [`ServerSlot::Ready`].
#[derive(Debug)]
enum ServerSlot {
    /// Spawning + initializing on a background thread.
    Starting(JoinHandle<io::Result<LanguageServer>>),
    /// Initialized and ready for sync/diagnostics.
    Ready(LanguageServer),
    /// Startup failed (e.g. the server program is not installed); not retried.
    Failed,
}

/// The result of an interactive LSP request, delivered back to the editor once a worker thread has
/// the server's reply. Drained each frame by [`LspManager::poll_outcomes`] (the request itself runs
/// off-thread so a slow server never blocks the render loop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspOutcome {
    /// Completion candidates for the document at `path`, ready to show in the popup.
    Completion {
        /// The document the request was issued for.
        path: PathBuf,
        /// The candidates, already converted to editor-facing items.
        items: Vec<CompletionItem>,
    },
    /// Hover documentation for the cursor in the document at `path`. `text` is `None` when the
    /// server reported nothing to show (the popup simply does not open).
    Hover {
        /// The document the request was issued for.
        path: PathBuf,
        /// The hover content reduced to plain text, or `None` when there is nothing to show.
        text: Option<String>,
    },
    /// The definition site for the cursor in the document at `path`. `target` is the destination
    /// file and LSP position, or `None` when the server found no definition (nothing happens).
    GotoDefinition {
        /// The document the request was issued for.
        path: PathBuf,
        /// The destination file + position, or `None` when there is nothing to jump to.
        target: Option<(PathBuf, Position)>,
    },
    /// A whole-document reformat for the document at `path`, computed off-thread by applying the
    /// server's edits to the text that was sent. `formatted` is the new document text, or `None`
    /// when there is nothing to change (the server returned no edits, errored, or the result is
    /// identical to the text formatted).
    Formatting {
        /// The document the request was issued for.
        path: PathBuf,
        /// The reformatted document text, or `None` when there is nothing to apply.
        formatted: Option<String>,
    },
}

/// Manages language servers and open-document sync.
#[derive(Debug)]
pub struct LspManager {
    /// Server launch config keyed by file extension (without the dot), e.g. `rs`.
    configs: HashMap<String, ServerConfig>,
    /// Servers keyed by `languageId`, each in its lifecycle state.
    servers: HashMap<String, ServerSlot>,
    /// Open documents keyed by path.
    docs: HashMap<PathBuf, DocState>,
    /// Sends interactive-request results from worker threads back to the editor frame.
    outcomes_tx: mpsc::Sender<LspOutcome>,
    /// The editor end of the outcome channel, drained by [`Self::poll_outcomes`].
    outcomes_rx: mpsc::Receiver<LspOutcome>,
}

impl Default for LspManager {
    fn default() -> Self {
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        Self {
            configs: HashMap::new(),
            servers: HashMap::new(),
            docs: HashMap::new(),
            outcomes_tx,
            outcomes_rx,
        }
    }
}

impl LspManager {
    /// An empty manager with no servers configured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A manager with the built-in defaults (rust-analyzer for `.rs`).
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut manager = Self::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        manager
    }

    /// Registers `config` for files with extension `ext` (no leading dot). A later call for the
    /// same extension replaces it (so the manifest overrides the defaults).
    pub fn configure(&mut self, ext: &str, config: ServerConfig) {
        self.configs.insert(ext.to_owned(), config);
    }

    /// Registers an already-initialized server for `language_id` as [`ServerSlot::Ready`] (tests
    /// inject a mock this way, bypassing process spawning + the handshake).
    pub fn register_server(&mut self, language_id: &str, server: LanguageServer) {
        self.servers
            .insert(language_id.to_owned(), ServerSlot::Ready(server));
    }

    /// Whether a server is configured for `path`'s extension.
    #[must_use]
    pub fn handles(&self, path: &Path) -> bool {
        self.config_for(path).is_some()
    }

    /// Opens `path` (content `text`) in its language's server, spawning + initializing the server
    /// on first use. A file with no configured server is ignored.
    ///
    /// # Errors
    /// Returns an I/O error spawning/initializing the server or sending the open notification.
    pub fn open(&mut self, path: &Path, text: &str) -> io::Result<()> {
        let Some(config) = self.config_for(path) else {
            return Ok(());
        };
        self.ensure_server_starting(&config, path);
        let uri = file_uri(path)?;
        self.docs.insert(
            path.to_owned(),
            DocState {
                language_id: config.language_id.clone(),
                version: 1,
                text: text.to_owned(),
                uri: uri.clone(),
            },
        );
        // If the server is already up, notify now; otherwise `poll` sends `didOpen` once it is
        // ready (with the document's then-current text, absorbing any edits made meanwhile).
        if let Some(ServerSlot::Ready(server)) = self.servers.get(&config.language_id) {
            server.did_open(uri, &config.language_id, 1, text.to_owned())?;
        }
        Ok(())
    }

    /// Notifies the server `path` changed to `text` (full-document sync). A no-op for a document
    /// that was never opened.
    ///
    /// # Errors
    /// Returns an I/O error sending the change notification.
    pub fn change(&mut self, path: &Path, text: &str) -> io::Result<()> {
        let Some(doc) = self.docs.get_mut(path) else {
            return Ok(());
        };
        doc.version += 1;
        text.clone_into(&mut doc.text);
        let (version, language_id, uri) = (doc.version, doc.language_id.clone(), doc.uri.clone());
        // Only a ready server is notified; a still-starting one picks up the new text via the
        // `didOpen` sent on ready.
        if let Some(ServerSlot::Ready(server)) = self.servers.get(&language_id) {
            server.did_change(uri, version, text.to_owned())?;
        }
        Ok(())
    }

    /// Advances any finished startups and drains every ready server's published diagnostics,
    /// returning them per document as byte-range [`Diagnostic`]s (converted against the document's
    /// current text). The host calls this each frame and applies each list to the matching editor.
    /// Non-blocking — joining a startup thread only happens once it has finished.
    pub fn poll(&mut self) -> Vec<(PathBuf, Vec<Diagnostic>)> {
        self.advance_startups();
        let mut updates = Vec::new();
        for slot in self.servers.values() {
            if let ServerSlot::Ready(server) = slot {
                for published in server.diagnostics() {
                    if let Some(update) = self.convert(&published) {
                        updates.push(update);
                    }
                }
            }
        }
        updates
    }

    /// Requests completion candidates for the cursor `byte` in `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. The request and
    /// its (bounded) wait run on a worker thread holding a shared [`Requester`]; the result arrives
    /// via [`Self::poll_outcomes`]. Issuing it never blocks the render loop.
    pub fn request_completion(&self, path: &Path, byte: usize) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let position = byte_to_position(&doc.text, byte);
        let uri = doc.uri.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let items = fetch_completion(&requester, uri, position);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::Completion { path, items });
        });
    }

    /// Requests hover documentation for the cursor `byte` in `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Like
    /// [`Self::request_completion`], the request and its bounded wait run on a worker thread holding
    /// a shared [`Requester`]; the result arrives via [`Self::poll_outcomes`] as [`LspOutcome::Hover`].
    pub fn request_hover(&self, path: &Path, byte: usize) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let position = byte_to_position(&doc.text, byte);
        let uri = doc.uri.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let text = fetch_hover(&requester, uri, position);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::Hover { path, text });
        });
    }

    /// Requests the definition site for the cursor `byte` in `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Like
    /// [`Self::request_hover`], the request and its bounded wait run on a worker thread holding a
    /// shared [`Requester`]; the result arrives via [`Self::poll_outcomes`] as
    /// [`LspOutcome::GotoDefinition`].
    pub fn request_goto_definition(&self, path: &Path, byte: usize) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let position = byte_to_position(&doc.text, byte);
        let uri = doc.uri.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let target = fetch_goto_definition(&requester, uri, position);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::GotoDefinition { path, target });
        });
    }

    /// Requests a whole-document reformat of `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Mirrors
    /// [`Self::request_goto_definition`]: the request and its bounded wait run on a worker thread
    /// holding a shared [`Requester`]; the result arrives via [`Self::poll_outcomes`] as
    /// [`LspOutcome::Formatting`]. The server's edits are applied off-thread to the exact text last
    /// sent to it (the document snapshot), so the host can replace the buffer wholesale as long as it
    /// is still on that revision.
    pub fn request_formatting(&self, path: &Path) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let uri = doc.uri.clone();
        let snapshot = doc.text.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let formatted = fetch_formatting(&requester, uri, &snapshot);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::Formatting { path, formatted });
        });
    }

    /// Drains the interactive-request results (completion, …) that have arrived since the last call.
    /// The host calls this each frame and applies each outcome (e.g. opens the completion popup).
    pub fn poll_outcomes(&mut self) -> Vec<LspOutcome> {
        self.outcomes_rx.try_iter().collect()
    }

    /// Promotes any background startup that has finished to [`ServerSlot::Ready`] (or `Failed`),
    /// sending `didOpen` for every already-open document of that language on success.
    fn advance_startups(&mut self) {
        let finished: Vec<String> = self
            .servers
            .iter()
            .filter(
                |(_, slot)| matches!(slot, ServerSlot::Starting(handle) if handle.is_finished()),
            )
            .map(|(language, _)| language.clone())
            .collect();
        for language in finished {
            let Some(ServerSlot::Starting(handle)) = self.servers.remove(&language) else {
                continue;
            };
            let slot = match handle.join() {
                Ok(Ok(server)) => {
                    for doc in self.docs.values() {
                        if doc.language_id == language {
                            let _ = server.did_open(
                                doc.uri.clone(),
                                &language,
                                doc.version,
                                doc.text.clone(),
                            );
                        }
                    }
                    ServerSlot::Ready(server)
                }
                _ => ServerSlot::Failed,
            };
            self.servers.insert(language, slot);
        }
    }

    fn convert(&self, published: &PublishDiagnosticsParams) -> Option<(PathBuf, Vec<Diagnostic>)> {
        // Match the published URI back to the document we opened (the URI is canonicalized, so it
        // need not equal the editor's path) and convert against that document's current text.
        let (path, doc) = self
            .docs
            .iter()
            .find(|(_, doc)| doc.uri.as_str() == published.uri.as_str())?;
        let diagnostics = published
            .diagnostics
            .iter()
            .map(|diagnostic| to_core_diagnostic(&doc.text, diagnostic))
            .collect();
        Some((path.clone(), diagnostics))
    }

    fn config_for(&self, path: &Path) -> Option<ServerConfig> {
        let extension = path.extension()?.to_str()?;
        self.configs.get(extension).cloned()
    }

    /// Starts a server for `config`'s language on a background thread if one is not already
    /// present. The thread spawns the process and runs the blocking `initialize` handshake; the
    /// editor keeps running and `poll` adopts the server once it is ready.
    fn ensure_server_starting(&mut self, config: &ServerConfig, path: &Path) {
        if self.servers.contains_key(&config.language_id) {
            return;
        }
        let command = config.command.clone();
        let args = config.args.clone();
        let root = project_root(path);
        let handle = thread::spawn(move || {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let server = LanguageServer::spawn(&command, &arg_refs)?;
            server.initialize(file_uri(&root)?)?;
            Ok(server)
        });
        self.servers
            .insert(config.language_id.clone(), ServerSlot::Starting(handle));
    }
}

/// Converts an LSP diagnostic to the editor's byte-range form against `text`.
fn to_core_diagnostic(text: &str, diagnostic: &LspDiagnostic) -> Diagnostic {
    let start = position_to_byte(
        text,
        diagnostic.range.start.line,
        diagnostic.range.start.character,
    );
    let end = position_to_byte(
        text,
        diagnostic.range.end.line,
        diagnostic.range.end.character,
    )
    .max(start);
    Diagnostic::new(
        start..end,
        severity_of(diagnostic.severity),
        diagnostic.message.clone(),
    )
}

/// Maps an LSP severity (defaulting to `Error` when unset) to the editor's [`Severity`].
fn severity_of(severity: Option<DiagnosticSeverity>) -> Severity {
    match severity {
        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(DiagnosticSeverity::INFORMATION) => Severity::Information,
        Some(DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::Error,
    }
}

/// Converts an LSP `(line, character)` position to a byte offset in `text`.
///
/// `character` is treated as a Unicode-scalar offset — exact for the BMP/ASCII; astral-plane
/// columns (which LSP counts as two UTF-16 units) are refined in a later pass. A line or column
/// past the end clamps to the end of the line / document.
///
/// Exposed so the host can map a server-reported position (e.g. a goto-definition target, or a
/// formatting edit's range) to a byte offset against the relevant document's text.
#[must_use]
pub fn position_to_byte(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    for (index, line_text) in text.split_inclusive('\n').enumerate() {
        if u32::try_from(index).unwrap_or(u32::MAX) == line {
            for (chars, (byte, _)) in line_text.char_indices().enumerate() {
                if u32::try_from(chars).unwrap_or(u32::MAX) == character {
                    return offset + byte;
                }
            }
            return offset + line_text.trim_end_matches('\n').len();
        }
        offset += line_text.len();
    }
    text.len()
}

/// Converts a byte offset in `text` to an LSP `(line, character)` position — the inverse of
/// [`position_to_byte`]. `character` is a Unicode-scalar offset within the line (exact for
/// BMP/ASCII). A byte on a line's trailing newline, or past the end of the document, clamps to the
/// end of that line's content / the final line.
fn byte_to_position(text: &str, byte: usize) -> Position {
    let mut offset = 0usize;
    for (index, line_text) in text.split_inclusive('\n').enumerate() {
        let content = line_text.trim_end_matches('\n');
        let content_len = content.len();
        let line = u32::try_from(index).unwrap_or(u32::MAX);
        if byte <= offset + content_len {
            // Within this line's content (or exactly at its end): count chars up to `byte`.
            let within = byte - offset;
            let character = u32::try_from(content[..within].chars().count()).unwrap_or(u32::MAX);
            return Position::new(line, character);
        }
        if byte < offset + line_text.len() {
            // On the trailing newline: clamp to the end of this line's content.
            let character = u32::try_from(content.chars().count()).unwrap_or(u32::MAX);
            return Position::new(line, character);
        }
        offset += line_text.len();
    }
    // Past the end (e.g. an empty final line after a trailing newline): clamp to the last line.
    let last_line = u32::try_from(text.split('\n').count().saturating_sub(1)).unwrap_or(u32::MAX);
    let last_chars = text.rsplit('\n').next().unwrap_or("").chars().count();
    Position::new(last_line, u32::try_from(last_chars).unwrap_or(u32::MAX))
}

/// Issues `textDocument/definition` over a shared [`Requester`] (on a worker thread) and reduces
/// the reply to a destination `(path, position)`. Handles all three response shapes (a single
/// `Location`, an array of them, or `LocationLink`s), taking the first. A timeout, transport
/// error, server error, `null`, or empty result all yield `None` (nothing happens).
fn fetch_goto_definition(
    requester: &Requester,
    uri: Uri,
    position: Position,
) -> Option<(PathBuf, Position)> {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let value = serde_json::to_value(params).ok()?;
    let response = requester
        .request_timeout("textDocument/definition", value, Duration::from_secs(3))
        .ok()?;
    let result = response.into_result().ok()?;
    // A `null` result (no definition) deserializes to `None`.
    let parsed: Option<GotoDefinitionResponse> = serde_json::from_value(result).unwrap_or(None);
    let (target_uri, target_position) = match parsed? {
        GotoDefinitionResponse::Scalar(location) => (location.uri, location.range.start),
        GotoDefinitionResponse::Array(locations) => {
            let first = locations.into_iter().next()?;
            (first.uri, first.range.start)
        }
        GotoDefinitionResponse::Link(links) => {
            let first = links.into_iter().next()?;
            (first.target_uri, first.target_selection_range.start)
        }
    };
    Some((uri_to_path(&target_uri)?, target_position))
}

/// Best-effort `file://` URI → filesystem path (v1 assumes no percent-encoding).
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    uri.as_str().strip_prefix("file://").map(PathBuf::from)
}

/// Issues `textDocument/formatting` over a shared [`Requester`] (on a worker thread) and applies the
/// returned edits to `snapshot` (the exact text last sent to the server), yielding the reformatted
/// document. A timeout, transport error, server error, a `null`/empty edit list, or a result
/// identical to `snapshot` all yield `None` (there is nothing to apply).
fn fetch_formatting(requester: &Requester, uri: Uri, snapshot: &str) -> Option<String> {
    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri },
        // Servers that honor these (rather than their own `rustfmt.toml`/`.editorconfig`) get the
        // LSP-required defaults; `tab_size`/`insert_spaces` have no `Default`, so they are explicit.
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..Default::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let value = serde_json::to_value(params).ok()?;
    let response = requester
        .request_timeout("textDocument/formatting", value, Duration::from_secs(3))
        .ok()?;
    let result = response.into_result().ok()?;
    // A `null` result (nothing to format) deserializes to `None`.
    let edits: Option<Vec<TextEdit>> = serde_json::from_value(result).unwrap_or(None);
    let edits = edits.filter(|edits| !edits.is_empty())?;
    let formatted = apply_text_edits(snapshot, edits);
    (formatted != snapshot).then_some(formatted)
}

/// Applies LSP [`TextEdit`]s to `text`, returning the new string. Each edit's range is mapped to byte
/// offsets via [`position_to_byte`] (so it lands on `char` boundaries), then the edits are spliced in
/// from the end of the document backwards (sorted by start offset, descending) so each splice leaves
/// the offsets of the not-yet-applied edits valid — the LSP spec guarantees the ranges do not
/// overlap.
fn apply_text_edits(text: &str, edits: Vec<TextEdit>) -> String {
    let mut spans: Vec<(usize, usize, String)> = edits
        .into_iter()
        .map(|edit| {
            let start = position_to_byte(text, edit.range.start.line, edit.range.start.character);
            let end =
                position_to_byte(text, edit.range.end.line, edit.range.end.character).max(start);
            (start, end, edit.new_text)
        })
        .collect();
    // Apply back-to-front: a later splice must not shift the offsets of an earlier (lower) one.
    spans.sort_by_key(|span| Reverse(span.0));
    let mut result = text.to_owned();
    for (start, end, new_text) in spans {
        result.replace_range(start..end, &new_text);
    }
    result
}

/// Issues `textDocument/completion` over a shared [`Requester`] (on a worker thread) and converts
/// the reply to editor-facing [`CompletionItem`]s. A timeout, transport error, server error, or
/// `null` result all yield an empty list (the popup simply does not open).
fn fetch_completion(requester: &Requester, uri: Uri, position: Position) -> Vec<CompletionItem> {
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) =
        requester.request_timeout("textDocument/completion", value, Duration::from_secs(3))
    else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (no completions) deserializes to `None`.
    let parsed: Option<CompletionResponse> = serde_json::from_value(result).unwrap_or(None);
    let items = match parsed {
        Some(
            CompletionResponse::Array(items)
            | CompletionResponse::List(CompletionList { items, .. }),
        ) => items,
        None => Vec::new(),
    };
    items
        .into_iter()
        .map(|item| {
            let label = item.label;
            let insert_text = item.insert_text.unwrap_or_else(|| label.clone());
            CompletionItem {
                label,
                insert_text,
                detail: item.detail,
            }
        })
        .collect()
}

/// Issues `textDocument/hover` over a shared [`Requester`] (on a worker thread) and reduces the
/// reply to plain text. A timeout, transport error, server error, `null` result, or empty content
/// all yield `None` (the popup simply does not open).
fn fetch_hover(requester: &Requester, uri: Uri, position: Position) -> Option<String> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let value = serde_json::to_value(params).ok()?;
    let response = requester
        .request_timeout("textDocument/hover", value, Duration::from_secs(3))
        .ok()?;
    let result = response.into_result().ok()?;
    // A `null` result (nothing to show) deserializes to `None`.
    let hover: Option<LspHover> = serde_json::from_value(result).unwrap_or(None);
    let text = hover_text(hover?.contents);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Reduces LSP [`HoverContents`] (markup, a marked string, or an array of them) to plain text.
fn hover_text(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked) => marked_string_text(marked),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(markup) => markup_text(markup),
    }
}

/// The text of a [`MarkedString`] — a bare string, or the code body of a language-tagged block.
fn marked_string_text(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(block) => block.value,
    }
}

/// The text of a [`MarkupContent`]. Markdown fence lines (```` ``` ````) are dropped so a TUI box
/// shows the code/prose they wrap rather than literal backticks; plain text passes through.
fn markup_text(markup: MarkupContent) -> String {
    if markup.kind == MarkupKind::Markdown {
        markup
            .value
            .lines()
            .filter(|line| !line.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        markup.value
    }
}

/// The project root the language server should be pointed at: the nearest ancestor of `path`
/// containing a `Cargo.toml`, falling back to `path`'s directory.
fn project_root(path: &Path) -> PathBuf {
    let start = path.parent().unwrap_or_else(|| Path::new("."));
    let mut dir = start;
    loop {
        if dir.join("Cargo.toml").is_file() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start.to_path_buf(),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{
        apply_text_edits, byte_to_position, markup_text, position_to_byte, LspManager, LspOutcome,
        ServerConfig,
    };
    use crate::client::LanguageServer;
    use crate::codec::{read_message, write_message};
    use crate::connection::Connection;

    #[test]
    fn position_maps_to_byte_offset() {
        let text = "let x = oops;\nfoo";
        assert_eq!(position_to_byte(text, 0, 0), 0);
        assert_eq!(position_to_byte(text, 0, 8), 8); // 'o' of oops
        assert_eq!(position_to_byte(text, 1, 0), 14); // start of "foo"
        assert_eq!(position_to_byte(text, 1, 3), 17); // end of "foo"
        assert_eq!(position_to_byte(text, 9, 0), text.len()); // past the end clamps
    }

    #[test]
    fn byte_to_position_is_the_inverse_of_position_to_byte() {
        for text in ["let x = oops;\nfoo", "abc", "a\n", "α = 1\nβ"] {
            // Every char-boundary byte must round-trip through (line, character) back to itself.
            for byte in 0..=text.len() {
                if !text.is_char_boundary(byte) {
                    continue;
                }
                let position = byte_to_position(text, byte);
                assert_eq!(
                    position_to_byte(text, position.line, position.character),
                    byte,
                    "byte {byte} of {text:?} round-tripped via {position:?}"
                );
            }
        }
    }

    #[test]
    fn completion_request_yields_mapped_items_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one `textDocument/completion` with two items, then idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            // `open` sends `didOpen` to the (already-ready) server first; skip notifications until
            // the completion request arrives.
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/completion" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {"label": "println!", "insertText": "println!", "detail": "macro"},
                        {"label": "print!"}
                    ]
                }),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let mut manager = LspManager::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));

        let path = std::path::Path::new("/tmp/does-not-exist-c.rs");
        manager.open(path, "let _ = pri").unwrap();
        manager.request_completion(path, "let _ = pri".len());

        // The worker replies asynchronously; poll until the outcome lands.
        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            outcomes = manager.poll_outcomes();
            if !outcomes.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(outcomes.len(), 1);
        let LspOutcome::Completion { path: got, items } = &outcomes[0] else {
            panic!("expected a completion outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "println!");
        assert_eq!(items[0].insert_text, "println!");
        assert_eq!(items[0].detail.as_deref(), Some("macro"));
        // A missing `insertText` falls back to the label.
        assert_eq!(items[1].label, "print!");
        assert_eq!(items[1].insert_text, "print!");
        assert_eq!(items[1].detail, None);

        mock.join().unwrap();
    }

    #[test]
    fn markup_text_strips_markdown_fences_but_keeps_plaintext() {
        use lsp_types::{MarkupContent, MarkupKind};

        let markdown = MarkupContent {
            kind: MarkupKind::Markdown,
            value: "```rust\nfn foo()\n```\n\nDocs".to_owned(),
        };
        assert_eq!(markup_text(markdown), "fn foo()\n\nDocs");

        let plain = MarkupContent {
            kind: MarkupKind::PlainText,
            value: "```not a fence```".to_owned(),
        };
        assert_eq!(markup_text(plain), "```not a fence```");
    }

    #[test]
    fn hover_request_yields_plain_text_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one `textDocument/hover` with fenced markdown, then idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            // `open` sends `didOpen` first; skip notifications until the hover request arrives.
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/hover" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": {
                            "kind": "markdown",
                            "value": "```rust\nfn foo()\n```\n\nDocs for foo"
                        }
                    }
                }),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let mut manager = LspManager::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));

        let path = std::path::Path::new("/tmp/does-not-exist-h.rs");
        manager.open(path, "let _ = foo").unwrap();
        manager.request_hover(path, "let _ = foo".len());

        // The worker replies asynchronously; poll until the outcome lands.
        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            outcomes = manager.poll_outcomes();
            if !outcomes.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(outcomes.len(), 1);
        let LspOutcome::Hover { path: got, text } = &outcomes[0] else {
            panic!("expected a hover outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        // Fences stripped, surrounding text preserved.
        assert_eq!(text.as_deref(), Some("fn foo()\n\nDocs for foo"));

        mock.join().unwrap();
    }

    #[test]
    fn published_diagnostics_become_byte_range_diagnostics() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that pushes a single error diagnostic for the document, then idles.
        let mock = thread::spawn(move || {
            let mut writer = server;
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": "file:///tmp/does-not-exist-x.rs",
                        "diagnostics": [{
                            "range": {"start": {"line": 0, "character": 8}, "end": {"line": 0, "character": 12}},
                            "severity": 1,
                            "message": "cannot find value `oops`"
                        }]
                    }
                }),
            )
            .unwrap();
            // Keep the socket open so the client's reader does not see EOF mid-test.
            thread::sleep(Duration::from_millis(300));
        });

        let mut manager = LspManager::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        // Inject the mock server (so `open` skips spawning rust-analyzer).
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));

        let path = std::path::Path::new("/tmp/does-not-exist-x.rs");
        manager.open(path, "let x = oops;").unwrap();

        // Diagnostics arrive asynchronously on the reader thread; poll until they land.
        let mut updates = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            updates = manager.poll();
            if !updates.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(updates.len(), 1);
        let (got_path, diagnostics) = &updates[0];
        assert_eq!(got_path, path);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range, 8..12); // "oops" as byte offsets
        assert_eq!(diagnostics[0].message, "cannot find value `oops`");

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_reports_a_diagnostic() {
        use std::fs;

        // A throwaway cargo project with a deliberate type error.
        let dir = std::env::temp_dir().join(format!("majestic-ra-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source = "fn main() {\n    let _x: i32 = \"a type error\";\n}\n";
        fs::write(&main_rs, source).unwrap();

        // The manager spawns + initializes rust-analyzer on a background thread; poll until it
        // finishes analyzing and publishes diagnostics (analysis takes a few seconds).
        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        let mut found = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            for (path, diagnostics) in manager.poll() {
                if path == main_rs && !diagnostics.is_empty() {
                    found = diagnostics;
                }
            }
            if !found.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        assert!(
            !found.is_empty(),
            "rust-analyzer should report at least one diagnostic for a type error"
        );
        // The byte ranges the editor underlines must be real spans within the document — this is
        // exactly the data the App applies to the buffer.
        assert!(
            found
                .iter()
                .any(|d| d.range.start < d.range.end && d.range.end <= source.len()),
            "a diagnostic should cover a non-empty span inside the document: {found:?}"
        );
    }

    #[test]
    fn goto_definition_request_yields_target_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one `textDocument/definition` with a single Location, idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            // `open` sends `didOpen` first; skip notifications until the definition request arrives.
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/definition" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "uri": "file:///tmp/does-not-exist-target.rs",
                        "range": {
                            "start": {"line": 2, "character": 4},
                            "end": {"line": 2, "character": 10}
                        }
                    }
                }),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let mut manager = LspManager::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));

        let path = std::path::Path::new("/tmp/does-not-exist-g.rs");
        manager.open(path, "let _ = foo").unwrap();
        manager.request_goto_definition(path, "let _ = foo".len());

        // The worker replies asynchronously; poll until the outcome lands.
        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            outcomes = manager.poll_outcomes();
            if !outcomes.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(outcomes.len(), 1);
        let LspOutcome::GotoDefinition { path: got, target } = &outcomes[0] else {
            panic!("expected a goto-definition outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        let (target_path, position) = target.as_ref().expect("a definition target");
        assert_eq!(
            target_path,
            std::path::Path::new("/tmp/does-not-exist-target.rs")
        );
        assert_eq!((position.line, position.character), (2, 4));

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_jumps_to_a_definition() {
        use std::fs;

        // A throwaway cargo project where `main` calls a local function `target`.
        let dir = std::env::temp_dir().join(format!("majestic-ra-gd-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source = "fn target() -> i32 {\n    42\n}\nfn main() {\n    let _ = target();\n}\n";
        fs::write(&main_rs, source).unwrap();
        // The cursor on the `target()` call (the last occurrence of the identifier).
        let call_byte = source.rfind("target").unwrap() + 1;

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        // Drive startup (`poll`) and keep issuing the request until the (async-started) server is
        // ready and replies. Goto-definition should resolve the call back to `fn target` on line 0.
        let mut target = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_goto_definition(&main_rs, call_byte); // a no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::GotoDefinition {
                    target: Some(found),
                    ..
                } = outcome
                {
                    target = Some(found);
                }
            }
            if target.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        let (target_path, position) = target.expect("rust-analyzer should resolve the definition");
        assert_eq!(target_path, main_rs, "definition is in the same file");
        assert_eq!(position.line, 0, "`fn target` is on line 0");
    }

    #[test]
    fn apply_text_edits_splices_in_reverse_offset_order() {
        use lsp_types::{Position, Range, TextEdit};

        // Two disjoint edits given in document order; applying the later (line 1) one first must
        // leave the earlier (line 0) one's offsets valid.
        let text = "foo = 1\nbar=2\n";
        let edits = vec![
            TextEdit {
                range: Range::new(Position::new(1, 3), Position::new(1, 4)), // the "=" in "bar=2"
                new_text: " = ".to_owned(),
            },
            TextEdit {
                range: Range::new(Position::new(0, 0), Position::new(0, 3)), // "foo"
                new_text: "FOO".to_owned(),
            },
        ];
        assert_eq!(apply_text_edits(text, edits), "FOO = 1\nbar = 2\n");
    }

    #[test]
    fn formatting_request_yields_formatted_text_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one `textDocument/formatting` with a single edit, then idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            // `open` sends `didOpen` first; skip notifications until the formatting request arrives.
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/formatting" {
                    break message["id"].clone();
                }
            };
            // Replace the whole (8-char) line "let  x=1" with the normalized "let x = 1".
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 8}
                        },
                        "newText": "let x = 1"
                    }]
                }),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let mut manager = LspManager::new();
        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));

        let path = std::path::Path::new("/tmp/does-not-exist-fmt.rs");
        manager.open(path, "let  x=1").unwrap();
        manager.request_formatting(path);

        // The worker replies asynchronously; poll until the outcome lands.
        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            outcomes = manager.poll_outcomes();
            if !outcomes.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(outcomes.len(), 1);
        let LspOutcome::Formatting {
            path: got,
            formatted,
        } = &outcomes[0]
        else {
            panic!("expected a formatting outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        assert_eq!(formatted.as_deref(), Some("let x = 1"));

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_formats_a_document() {
        use std::fs;

        // A throwaway cargo project whose `main.rs` is deliberately badly formatted.
        let dir = std::env::temp_dir().join(format!("majestic-ra-fmt-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source = "fn main(){let _x=1;}\n";
        fs::write(&main_rs, source).unwrap();

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        // Drive startup (`poll`) and keep requesting until the (async-started) server is ready and
        // returns a reformat.
        let mut formatted = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_formatting(&main_rs); // a no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::Formatting {
                    formatted: Some(text),
                    ..
                } = outcome
                {
                    formatted = Some(text);
                }
            }
            if formatted.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        let formatted = formatted.expect("rust-analyzer should return a reformatted document");
        assert!(
            formatted.contains("fn main() {"),
            "rustfmt should space the signature: {formatted:?}"
        );
        assert_ne!(formatted, source, "formatting should change the source");
    }
}
