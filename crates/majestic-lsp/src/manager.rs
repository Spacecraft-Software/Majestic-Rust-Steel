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
    CodeActionContext, CodeActionOrCommand, CodeActionParams, CompletionList, CompletionParams,
    CompletionResponse, Diagnostic as LspDiagnostic, DiagnosticSeverity, DocumentChangeOperation,
    DocumentChanges, DocumentFormattingParams, DocumentHighlight as LspDocumentHighlight,
    DocumentHighlightKind, DocumentHighlightParams, DocumentSymbol, DocumentSymbolParams,
    DocumentSymbolResponse, FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse,
    Hover as LspHover, HoverContents, HoverParams, Location, MarkedString, MarkupContent,
    MarkupKind, OneOf, ParameterLabel, PartialResultParams, Position, PublishDiagnosticsParams,
    Range as LspRange, ReferenceContext, ReferenceParams, RenameParams,
    SignatureHelp as LspSignatureHelp, SignatureHelpParams, SymbolKind, TextDocumentEdit,
    TextDocumentIdentifier, TextDocumentPositionParams, TextEdit, Uri, WorkDoneProgressParams,
    WorkspaceEdit,
};
use majestic_core::{
    CodeAction, CompletionItem, Diagnostic, Occurrence, Reference, RenameEdit, Severity,
    SignatureHelp, Symbol,
};

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
    /// The raw LSP diagnostics last published for this document, retained so a `codeAction` request
    /// can pass the ones covering the cursor as its context (which is how quick-fixes are offered).
    diagnostics: Vec<LspDiagnostic>,
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

/// The lifecycle health of a language server, for a status indicator ([`LspManager::server_health`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerHealth {
    /// Spawning + running the `initialize` handshake (not yet usable).
    Starting,
    /// Initialized and serving requests.
    Ready,
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
    /// Signature help for the call the cursor is inside in the document at `path`. `signature` is
    /// `None` when the cursor is not in a call (or the server returned nothing) — the popup closes.
    SignatureHelp {
        /// The document the request was issued for.
        path: PathBuf,
        /// The active signature + highlighted parameter, or `None` to close the popup.
        signature: Option<SignatureHelp>,
    },
    /// The definition site for the cursor in the document at `path`. `target` is the destination
    /// file and LSP position, or `None` when the server found no definition (nothing happens).
    GotoDefinition {
        /// The document the request was issued for.
        path: PathBuf,
        /// The destination file + position, or `None` when there is nothing to jump to.
        target: Option<(PathBuf, Position)>,
    },
    /// Every use site of the symbol under the cursor in the document at `path`, ready to show in the
    /// references popup. Empty when the server found none (the popup does not open).
    References {
        /// The document the request was issued for.
        path: PathBuf,
        /// The use sites, each with a destination file/position and a source-line preview.
        references: Vec<Reference>,
    },
    /// Occurrences of the symbol under the cursor *within* the document at `path` (LSP
    /// `documentHighlight`), as byte ranges to tint. Empty when the cursor is not on a symbol.
    DocumentHighlight {
        /// The document the request was issued for.
        path: PathBuf,
        /// The occurrences (byte ranges + read/write), tinted in the buffer by the host.
        occurrences: Vec<Occurrence>,
    },
    /// The code actions (quick-fixes / refactors) the server offers at the cursor in the document at
    /// `path`, ready to show in the menu. Empty when the server offers none (the menu does not open).
    CodeActions {
        /// The document the request was issued for.
        path: PathBuf,
        /// The offered actions, each with a title and its already-reduced edits.
        actions: Vec<CodeAction>,
    },
    /// The outline of every definition in the document at `path` (functions, types, members, …),
    /// flattened in document order with nesting depth, ready to show in the symbols picker. Empty
    /// when the server found none (the picker does not open).
    DocumentSymbols {
        /// The document the request was issued for.
        path: PathBuf,
        /// The definitions, each with a same-file position, a kind badge, and a nesting depth.
        symbols: Vec<Symbol>,
    },
    /// The edits to apply for a rename triggered in the document at `path` — a flat list spanning
    /// every affected file. Empty when the server rejected the rename (e.g. not a renameable symbol).
    Rename {
        /// The document the rename was triggered from (used to keep focus there afterward).
        path: PathBuf,
        /// The replacements across all files, applied by the host back-to-front per file.
        edits: Vec<RenameEdit>,
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

    /// The language server for `path`'s language: its command name and current health, for a status
    /// indicator. `None` when no server is configured for the extension. A configured language whose
    /// server has not been spawned yet (e.g. before the file is opened) reads as
    /// [`ServerHealth::Starting`].
    #[must_use]
    pub fn server_health(&self, path: &Path) -> Option<(String, ServerHealth)> {
        let config = self.config_for(path)?;
        let health = match self.servers.get(&config.language_id) {
            Some(ServerSlot::Ready(_)) => ServerHealth::Ready,
            Some(ServerSlot::Failed) => ServerHealth::Failed,
            Some(ServerSlot::Starting(_)) | None => ServerHealth::Starting,
        };
        Some((config.command, health))
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
                diagnostics: Vec::new(),
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
        // Drain every ready server's published diagnostics first, releasing the `servers` borrow
        // before touching `docs` (which we mutate to retain the raw diagnostics for codeAction).
        let published: Vec<PublishDiagnosticsParams> = self
            .servers
            .values()
            .filter_map(|slot| match slot {
                ServerSlot::Ready(server) => Some(server.diagnostics()),
                _ => None,
            })
            .flatten()
            .collect();
        let mut updates = Vec::new();
        for params in published {
            // Match the published URI back to an open document (canonicalized, so it need not equal
            // the editor's path); convert to byte ranges for the editor and retain the raw ones.
            let Some((path, doc)) = self
                .docs
                .iter_mut()
                .find(|(_, doc)| doc.uri.as_str() == params.uri.as_str())
            else {
                continue;
            };
            let diagnostics = params
                .diagnostics
                .iter()
                .map(|diagnostic| to_core_diagnostic(&doc.text, diagnostic))
                .collect();
            doc.diagnostics = params.diagnostics;
            updates.push((path.clone(), diagnostics));
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

    /// Requests signature help for the cursor `byte` in `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Like
    /// [`Self::request_hover`], the request and its bounded wait run on a worker thread holding a
    /// shared [`Requester`]; the result arrives via [`Self::poll_outcomes`] as
    /// [`LspOutcome::SignatureHelp`] — `Some` to show/update the popup, `None` to close it.
    pub fn request_signature_help(&self, path: &Path, byte: usize) {
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
            let signature = fetch_signature_help(&requester, uri, position);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::SignatureHelp { path, signature });
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

    /// Requests every use site of the symbol at the cursor `byte` in `path`, off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Like
    /// [`Self::request_goto_definition`], the request and its bounded wait run on a worker thread
    /// holding a shared [`Requester`]; the result arrives via [`Self::poll_outcomes`] as
    /// [`LspOutcome::References`]. The worker also reads each hit's source line (off-thread) to build
    /// the list preview.
    pub fn request_references(&self, path: &Path, byte: usize) {
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
            let references = fetch_references(&requester, uri, position);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::References { path, references });
        });
    }

    /// Requests the occurrences of the symbol at the cursor `byte` in `path` (LSP
    /// `documentHighlight`), off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. The worker
    /// converts the server's positions to byte offsets against the document snapshot captured here
    /// (the same text the server is reasoning about), and the result arrives via
    /// [`Self::poll_outcomes`] as [`LspOutcome::DocumentHighlight`]. The host auto-issues this as the
    /// cursor moves, so it is deliberately lightweight.
    pub fn request_document_highlight(&self, path: &Path, byte: usize) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let position = byte_to_position(&doc.text, byte);
        let uri = doc.uri.clone();
        let snapshot = doc.text.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let occurrences = fetch_document_highlight(&requester, uri, position, &snapshot);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::DocumentHighlight { path, occurrences });
        });
    }

    /// Requests the code actions (quick-fixes / refactors) offered at the cursor `byte` in `path`,
    /// off the editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. The request's
    /// context carries the document's diagnostics that cover the cursor (so the server can offer
    /// their quick-fixes); the result arrives via [`Self::poll_outcomes`] as
    /// [`LspOutcome::CodeActions`].
    pub fn request_code_action(&self, path: &Path, byte: usize) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let position = byte_to_position(&doc.text, byte);
        // The diagnostics covering the cursor become the action context (quick-fix sources).
        let diagnostics: Vec<LspDiagnostic> = doc
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic_covers(&diagnostic.range, position))
            .cloned()
            .collect();
        let uri = doc.uri.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let range = LspRange::new(position, position); // zero-width range at the cursor
            let actions = fetch_code_actions(&requester, uri, range, diagnostics);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::CodeActions { path, actions });
        });
    }

    /// Requests the outline of every definition in `path` (functions, types, members, …), off the
    /// editor thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Whole-document,
    /// so unlike the cursor-based requests it takes no `byte`. The request and its bounded wait run
    /// on a worker thread holding a shared [`Requester`]; the result arrives via
    /// [`Self::poll_outcomes`] as [`LspOutcome::DocumentSymbols`].
    pub fn request_document_symbols(&self, path: &Path) {
        let Some(doc) = self.docs.get(path) else {
            return;
        };
        let Some(ServerSlot::Ready(server)) = self.servers.get(&doc.language_id) else {
            return;
        };
        let requester = server.requester();
        let uri = doc.uri.clone();
        let path = path.to_owned();
        let tx = self.outcomes_tx.clone();
        thread::spawn(move || {
            let symbols = fetch_document_symbols(&requester, uri);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::DocumentSymbols { path, symbols });
        });
    }

    /// Requests a rename of the symbol at the cursor `byte` in `path` to `new_name`, off the editor
    /// thread.
    ///
    /// A no-op unless the document is open and its server is [`ServerSlot::Ready`]. Like the other
    /// interactive requests, the request and its bounded wait run on a worker thread holding a shared
    /// [`Requester`]; the result arrives via [`Self::poll_outcomes`] as [`LspOutcome::Rename`] — a flat
    /// list of edits spanning every affected file, which the host applies.
    pub fn request_rename(&self, path: &Path, byte: usize, new_name: String) {
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
            let edits = fetch_rename(&requester, uri, position, new_name);
            // The receiver is gone only if the editor has shut down; dropping the result is fine.
            let _ = tx.send(LspOutcome::Rename { path, edits });
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

/// Largest file the preview reader will open. A reference into a generated/vendored file can point
/// at a multi-megabyte source; capping the read keeps one slow `open` from stalling the worker.
const MAX_PREVIEW_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB — ample for hand-written source.

/// Issues `textDocument/references` over a shared [`Requester`] (on a worker thread) and converts the
/// returned [`Location`]s to editor-facing [`Reference`]s, including the declaration itself. Each
/// hit's source line is read from disk (off-thread) for the list preview, reading any one file at
/// most once. A timeout, transport error, server error, `null`, or empty result all yield an empty
/// list (the popup does not open). The preview is best-effort: it reflects the file on disk, so a hit
/// in a buffer with unsaved edits may show a slightly stale line — the navigation target is always
/// exact, since the jump re-resolves the position against the destination buffer's current text.
fn fetch_references(requester: &Requester, uri: Uri, position: Position) -> Vec<Reference> {
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        // Include the declaration so the list shows the definition alongside the uses.
        context: ReferenceContext {
            include_declaration: true,
        },
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) =
        requester.request_timeout("textDocument/references", value, Duration::from_secs(3))
    else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (no references) deserializes to `None`.
    let locations: Option<Vec<Location>> = serde_json::from_value(result).unwrap_or(None);
    let Some(locations) = locations else {
        return Vec::new();
    };
    // Cache each file's contents so a symbol used many times in one file is read once.
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    locations
        .into_iter()
        .filter_map(|location| {
            let path = uri_to_path(&location.uri)?;
            let line = location.range.start.line;
            let preview = preview_line(&mut cache, &path, line);
            Some(Reference {
                path,
                line,
                character: location.range.start.character,
                preview,
            })
        })
        .collect()
}

/// The trimmed text of `line` (zero-based) in `path`, for a reference's list preview. Reads `path`
/// at most once via `cache`. Returns an empty string when the file is unreadable, too large, or has
/// no such line — the reference still navigates correctly.
fn preview_line(cache: &mut HashMap<PathBuf, Option<String>>, path: &Path, line: u32) -> String {
    let contents = cache
        .entry(path.to_owned())
        .or_insert_with(|| read_capped(path));
    let Some(text) = contents.as_deref() else {
        return String::new();
    };
    let index = usize::try_from(line).unwrap_or(usize::MAX);
    text.lines()
        .nth(index)
        .map(|line| line.trim().to_owned())
        .unwrap_or_default()
}

/// Reads `path` to a string, but only when it is a regular file no larger than
/// [`MAX_PREVIEW_FILE_BYTES`]. `None` when it is not a file, is too big, or cannot be read.
fn read_capped(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PREVIEW_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Issues `textDocument/documentHighlight` over a shared [`Requester`] (on a worker thread) and
/// converts the returned ranges to editor-facing [`Occurrence`]s — byte ranges into `snapshot` (the
/// document text the server is reasoning about), with `write` set for a `Write` occurrence. A short
/// (2 s) timeout, since the host issues this frequently as the cursor moves. A timeout, transport
/// error, server error, `null`, or empty result all yield an empty list (the tint clears).
fn fetch_document_highlight(
    requester: &Requester,
    uri: Uri,
    position: Position,
    snapshot: &str,
) -> Vec<Occurrence> {
    let params = DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) = requester.request_timeout(
        "textDocument/documentHighlight",
        value,
        Duration::from_secs(2),
    ) else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (cursor not on a symbol) deserializes to `None`.
    let highlights: Option<Vec<LspDocumentHighlight>> =
        serde_json::from_value(result).unwrap_or(None);
    let Some(highlights) = highlights else {
        return Vec::new();
    };
    highlights
        .into_iter()
        .map(|highlight| {
            let start = position_to_byte(
                snapshot,
                highlight.range.start.line,
                highlight.range.start.character,
            );
            let end = position_to_byte(
                snapshot,
                highlight.range.end.line,
                highlight.range.end.character,
            )
            .max(start);
            let write = highlight.kind == Some(DocumentHighlightKind::WRITE);
            Occurrence::new(start..end, write)
        })
        .collect()
}

/// Whether LSP `position` falls within `range` (inclusive of the end, so a cursor at a diagnostic's
/// trailing edge still counts — quick-fixes commonly apply at the boundary).
fn diagnostic_covers(range: &LspRange, position: Position) -> bool {
    let after_start = position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character);
    let before_end = position.line < range.end.line
        || (position.line == range.end.line && position.character <= range.end.character);
    after_start && before_end
}

/// Issues `textDocument/codeAction` over a shared [`Requester`] (on a worker thread) and reduces the
/// reply to editor-facing [`CodeAction`]s. Each `CodeAction` carries its inline `WorkspaceEdit`
/// reduced to edits (we advertise literal support without resolve, so the server resolves them
/// eagerly); a legacy `Command` becomes an editless action (shown but not applied in v1). A timeout,
/// transport error, server error, `null`, or empty result all yield an empty list (no menu).
fn fetch_code_actions(
    requester: &Requester,
    uri: Uri,
    range: LspRange,
    diagnostics: Vec<LspDiagnostic>,
) -> Vec<CodeAction> {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri },
        range,
        context: CodeActionContext {
            diagnostics,
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) =
        requester.request_timeout("textDocument/codeAction", value, Duration::from_secs(3))
    else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (no actions) deserializes to `None`.
    let actions: Option<Vec<CodeActionOrCommand>> = serde_json::from_value(result).unwrap_or(None);
    let Some(actions) = actions else {
        return Vec::new();
    };
    actions
        .into_iter()
        .map(|item| match item {
            CodeActionOrCommand::CodeAction(action) => {
                let edits = action.edit.map(workspace_edit_to_edits).unwrap_or_default();
                CodeAction::new(action.title, edits)
            }
            CodeActionOrCommand::Command(command) => CodeAction::new(command.title, Vec::new()),
        })
        .collect()
}

/// Issues `textDocument/documentSymbol` over a shared [`Requester`] (on a worker thread) and
/// flattens the reply to editor-facing [`Symbol`]s in document order. Handles both response shapes:
/// the hierarchical `Nested` tree (pre-order, recording nesting depth) and the legacy flat list (all
/// at depth 0). A timeout, transport error, server error, `null`, or empty result all yield an empty
/// list (the picker does not open).
fn fetch_document_symbols(requester: &Requester, uri: Uri) -> Vec<Symbol> {
    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) =
        requester.request_timeout("textDocument/documentSymbol", value, Duration::from_secs(3))
    else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (no symbols) deserializes to `None`.
    let parsed: Option<DocumentSymbolResponse> = serde_json::from_value(result).unwrap_or(None);
    let mut symbols = Vec::new();
    match parsed {
        Some(DocumentSymbolResponse::Nested(nodes)) => {
            for node in &nodes {
                flatten_symbol(node, 0, &mut symbols);
            }
        }
        Some(DocumentSymbolResponse::Flat(infos)) => {
            // The legacy flat shape has no hierarchy; every symbol sits at depth 0. (The field-only
            // access here never names the deprecated `SymbolInformation` type.)
            symbols.extend(infos.into_iter().map(|info| Symbol {
                name: info.name,
                kind: symbol_kind_glyph(info.kind),
                line: info.location.range.start.line,
                character: info.location.range.start.character,
                depth: 0,
            }));
        }
        None => {}
    }
    symbols
}

/// Appends `node` and its descendants to `out` in pre-order, so children read directly under their
/// parent with an increasing nesting `depth`. The jump position is the symbol's `selection_range`
/// start — the name itself, not the whole body the `range` would span.
fn flatten_symbol(node: &DocumentSymbol, depth: u16, out: &mut Vec<Symbol>) {
    out.push(Symbol {
        name: node.name.clone(),
        kind: symbol_kind_glyph(node.kind),
        line: node.selection_range.start.line,
        character: node.selection_range.start.character,
        depth,
    });
    if let Some(children) = node.children.as_ref() {
        for child in children {
            flatten_symbol(child, depth.saturating_add(1), out);
        }
    }
}

/// A one-character badge for an LSP [`SymbolKind`], grouping the ~26 kinds into the handful that read
/// clearly in a TUI list: `m` module-like, `s` struct/class, `e` enum (+ members), `t` trait/
/// interface, `f` function/method, `c` constant, `v` variable, `.` field/property, `T` type
/// parameter, `·` everything else.
fn symbol_kind_glyph(kind: SymbolKind) -> char {
    match kind {
        SymbolKind::FILE | SymbolKind::MODULE | SymbolKind::NAMESPACE | SymbolKind::PACKAGE => 'm',
        SymbolKind::CLASS | SymbolKind::STRUCT | SymbolKind::OBJECT => 's',
        SymbolKind::ENUM | SymbolKind::ENUM_MEMBER => 'e',
        SymbolKind::INTERFACE => 't',
        SymbolKind::METHOD | SymbolKind::FUNCTION | SymbolKind::CONSTRUCTOR => 'f',
        SymbolKind::CONSTANT => 'c',
        SymbolKind::VARIABLE => 'v',
        SymbolKind::FIELD | SymbolKind::PROPERTY => '.',
        SymbolKind::TYPE_PARAMETER => 'T',
        _ => '·',
    }
}

/// Issues `textDocument/rename` over a shared [`Requester`] (on a worker thread) and reduces the
/// returned `WorkspaceEdit` to a flat list of editor-facing [`RenameEdit`]s (positions, not yet
/// applied — the host applies them against each file's live text, so an open buffer's unsaved edits
/// stay correct). Handles both `WorkspaceEdit` shapes: the simple `changes` map and the richer
/// `document_changes` (preferred when present). A timeout, transport error, server error, `null`, or
/// empty result all yield an empty list (nothing changes). Uses a slightly longer (5 s) timeout than
/// the read-only requests, since a project-wide rename can take the server longer to compute.
fn fetch_rename(
    requester: &Requester,
    uri: Uri,
    position: Position,
    new_name: String,
) -> Vec<RenameEdit> {
    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        new_name,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let Ok(value) = serde_json::to_value(params) else {
        return Vec::new();
    };
    let Ok(response) =
        requester.request_timeout("textDocument/rename", value, Duration::from_secs(5))
    else {
        return Vec::new();
    };
    let Ok(result) = response.into_result() else {
        return Vec::new();
    };
    // A `null` result (rename refused) deserializes to `None`.
    let edit: Option<WorkspaceEdit> = serde_json::from_value(result).unwrap_or(None);
    edit.map(workspace_edit_to_edits).unwrap_or_default()
}

/// Reduces an LSP [`WorkspaceEdit`] to a flat list of editor-facing [`RenameEdit`]s, handling both
/// shapes — the simple `changes` map and the richer `document_changes` (preferred when present, with
/// file create/rename/delete operations skipped). Shared by rename and code actions, which both
/// apply a server-provided `WorkspaceEdit`.
fn workspace_edit_to_edits(edit: WorkspaceEdit) -> Vec<RenameEdit> {
    let mut out = Vec::new();
    if let Some(changes) = edit.document_changes {
        // The richer form: a list of per-document edit batches (with optional file operations,
        // which we do not apply, so they are skipped).
        let doc_edits: Vec<TextDocumentEdit> = match changes {
            DocumentChanges::Edits(edits) => edits,
            DocumentChanges::Operations(ops) => ops
                .into_iter()
                .filter_map(|op| match op {
                    DocumentChangeOperation::Edit(edit) => Some(edit),
                    DocumentChangeOperation::Op(_) => None,
                })
                .collect(),
        };
        for doc_edit in doc_edits {
            let Some(path) = uri_to_path(&doc_edit.text_document.uri) else {
                continue;
            };
            for edit in doc_edit.edits {
                // Each edit is either a plain `TextEdit` or an annotated one; take the edit itself.
                let text_edit = match edit {
                    OneOf::Left(edit) => edit,
                    OneOf::Right(annotated) => annotated.text_edit,
                };
                out.push(rename_edit(path.clone(), &text_edit));
            }
        }
    } else if let Some(changes) = edit.changes {
        // The simple form: a map of file URI → edits (what a server returns when the client does not
        // advertise `documentChanges`, as here).
        for (uri, text_edits) in changes {
            let Some(path) = uri_to_path(&uri) else {
                continue;
            };
            for text_edit in text_edits {
                out.push(rename_edit(path.clone(), &text_edit));
            }
        }
    }
    out
}

/// Builds an editor-facing [`RenameEdit`] for `path` from an LSP [`TextEdit`].
fn rename_edit(path: PathBuf, edit: &TextEdit) -> RenameEdit {
    RenameEdit {
        path,
        start_line: edit.range.start.line,
        start_character: edit.range.start.character,
        end_line: edit.range.end.line,
        end_character: edit.range.end.character,
        new_text: edit.new_text.clone(),
    }
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

/// Issues `textDocument/signatureHelp` over a shared [`Requester`] (on a worker thread) and reduces
/// the reply to the active signature's label plus the byte range of the active parameter within it.
/// A timeout, transport error, server error, `null` (not in a call), an empty signature list, or a
/// blank label all yield `None` (the popup closes / does not open).
fn fetch_signature_help(
    requester: &Requester,
    uri: Uri,
    position: Position,
) -> Option<SignatureHelp> {
    let params = SignatureHelpParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        context: None,
    };
    let value = serde_json::to_value(params).ok()?;
    let response = requester
        .request_timeout("textDocument/signatureHelp", value, Duration::from_secs(3))
        .ok()?;
    let result = response.into_result().ok()?;
    // A `null` result (not in a call) deserializes to `None`.
    let help: Option<LspSignatureHelp> = serde_json::from_value(result).unwrap_or(None);
    let help = help?;
    // The active signature (default the first); then its active parameter (per-signature index,
    // falling back to the reply's top-level index).
    let signature = help
        .signatures
        .get(usize::try_from(help.active_signature.unwrap_or(0)).unwrap_or(0))?;
    let label = signature.label.clone();
    if label.trim().is_empty() {
        return None;
    }
    let active = signature
        .active_parameter
        .or(help.active_parameter)
        .and_then(|index| {
            signature
                .parameters
                .as_ref()?
                .get(usize::try_from(index).unwrap_or(usize::MAX))
        })
        .and_then(|parameter| parameter_byte_range(&label, &parameter.label));
    Some(SignatureHelp::new(label, active))
}

/// The byte range of a parameter within the signature `label`, from its LSP [`ParameterLabel`]: a
/// [`ParameterLabel::Simple`] substring is located by search; [`ParameterLabel::LabelOffsets`] are
/// character offsets into the label, converted to byte offsets (exact for BMP/ASCII labels). `None`
/// when the parameter cannot be located (so nothing is highlighted).
fn parameter_byte_range(label: &str, parameter: &ParameterLabel) -> Option<(usize, usize)> {
    match parameter {
        ParameterLabel::Simple(text) => label
            .find(text.as_str())
            .map(|start| (start, start + text.len())),
        ParameterLabel::LabelOffsets([start, end]) => {
            let char_to_byte = |char_index: u32| {
                label
                    .char_indices()
                    .nth(usize::try_from(char_index).unwrap_or(usize::MAX))
                    .map_or(label.len(), |(byte, _)| byte)
            };
            let (start, end) = (char_to_byte(*start), char_to_byte(*end));
            (start < end).then_some((start, end))
        }
    }
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
        apply_text_edits, byte_to_position, markup_text, parameter_byte_range, position_to_byte,
        read_capped, symbol_kind_glyph, LspManager, LspOutcome, ServerConfig, ServerHealth,
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

    #[test]
    fn references_request_yields_use_sites_with_previews_via_poll_outcomes() {
        use std::fs;

        // A real on-disk file the references point into, so the worker's preview reader has content
        // to extract (and both hits share it, exercising the per-file read cache).
        let target = std::env::temp_dir().join(format!("majestic-refs-{}.rs", std::process::id()));
        fs::write(&target, "fn parse(s: &str) {}\nlet _ = parse(raw);\n").unwrap();
        let response_uri = format!("file://{}", target.display());

        let (client, server) = UnixStream::pair().unwrap();
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            // `open` sends `didOpen` first; skip notifications until the references request arrives.
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/references" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {"uri": response_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}},
                        {"uri": response_uri, "range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 13}}}
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

        let path = std::path::Path::new("/tmp/does-not-exist-r.rs");
        manager.open(path, "let _ = parse").unwrap();
        manager.request_references(path, "let _ = parse".len());

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

        let _ = fs::remove_file(&target);

        assert_eq!(outcomes.len(), 1);
        let LspOutcome::References {
            path: got,
            references,
        } = &outcomes[0]
        else {
            panic!("expected a references outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        assert_eq!(references.len(), 2);
        // Locations map to (path, line, character); previews are the trimmed source lines.
        assert_eq!(references[0].path, target);
        assert_eq!((references[0].line, references[0].character), (0, 3));
        assert_eq!(references[0].preview, "fn parse(s: &str) {}");
        assert_eq!((references[1].line, references[1].character), (1, 8));
        assert_eq!(references[1].preview, "let _ = parse(raw);");

        mock.join().unwrap();
    }

    #[test]
    fn read_capped_returns_none_for_a_missing_file() {
        let missing =
            std::env::temp_dir().join(format!("majestic-no-such-{}.rs", std::process::id()));
        assert!(read_capped(&missing).is_none());
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_finds_references() {
        use std::fs;

        // A throwaway cargo project where `target` is declared once and called twice.
        let dir = std::env::temp_dir().join(format!("majestic-ra-refs-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source =
            "fn target() -> i32 {\n    42\n}\nfn main() {\n    let _ = target();\n    let _ = target();\n}\n";
        fs::write(&main_rs, source).unwrap();
        // The cursor on the `fn target` declaration (the first occurrence of the identifier).
        let decl_byte = source.find("target").unwrap() + 1;

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        // Drive startup (`poll`) and keep issuing the request until the (async-started) server is
        // ready and replies with the use sites.
        let mut references = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_references(&main_rs, decl_byte); // a no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::References {
                    references: found, ..
                } = outcome
                {
                    if !found.is_empty() {
                        references = found;
                    }
                }
            }
            if !references.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        // At least the two calls must be reported (the declaration may be included too).
        assert!(
            references.len() >= 2,
            "rust-analyzer should report the uses of `target`: {references:?}"
        );
        assert!(
            references.iter().all(|reference| reference.path == main_rs),
            "all references are in the same file"
        );
    }

    #[test]
    fn document_symbols_request_yields_flattened_outline_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one textDocument/documentSymbol with a nested outline: a struct
        // `Foo` containing a method `bar`, plus a top-level function `baz`. Then it idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/documentSymbol" {
                    break message["id"].clone();
                }
            };
            // SymbolKind 23 = STRUCT, 6 = METHOD, 12 = FUNCTION.
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {
                            "name": "Foo", "kind": 23,
                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 4, "character": 1}},
                            "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10}},
                            "children": [
                                {
                                    "name": "bar", "kind": 6,
                                    "range": {"start": {"line": 1, "character": 4}, "end": {"line": 3, "character": 5}},
                                    "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 10}}
                                }
                            ]
                        },
                        {
                            "name": "baz", "kind": 12,
                            "range": {"start": {"line": 6, "character": 0}, "end": {"line": 8, "character": 1}},
                            "selectionRange": {"start": {"line": 6, "character": 3}, "end": {"line": 6, "character": 6}}
                        }
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

        let path = std::path::Path::new("/tmp/does-not-exist-ds.rs");
        manager.open(path, "struct Foo;").unwrap();
        manager.request_document_symbols(path);

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
        let LspOutcome::DocumentSymbols { path: got, symbols } = &outcomes[0] else {
            panic!("expected a document-symbols outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        // Pre-order, flattened with depth: Foo (struct, 0) → bar (method, 1) → baz (function, 0).
        assert_eq!(symbols.len(), 3);
        assert_eq!(
            (symbols[0].name.as_str(), symbols[0].kind, symbols[0].depth),
            ("Foo", 's', 0)
        );
        // The jump position is the selection-range start (the name), not the body's range.
        assert_eq!((symbols[0].line, symbols[0].character), (0, 7));
        assert_eq!(
            (symbols[1].name.as_str(), symbols[1].kind, symbols[1].depth),
            ("bar", 'f', 1)
        );
        assert_eq!((symbols[1].line, symbols[1].character), (1, 7));
        assert_eq!(
            (symbols[2].name.as_str(), symbols[2].kind, symbols[2].depth),
            ("baz", 'f', 0)
        );

        mock.join().unwrap();
    }

    #[test]
    fn symbol_kind_glyph_groups_kinds() {
        use lsp_types::SymbolKind;
        assert_eq!(symbol_kind_glyph(SymbolKind::STRUCT), 's');
        assert_eq!(symbol_kind_glyph(SymbolKind::METHOD), 'f');
        assert_eq!(symbol_kind_glyph(SymbolKind::FUNCTION), 'f');
        assert_eq!(symbol_kind_glyph(SymbolKind::ENUM), 'e');
        assert_eq!(symbol_kind_glyph(SymbolKind::INTERFACE), 't');
        assert_eq!(symbol_kind_glyph(SymbolKind::MODULE), 'm');
        assert_eq!(symbol_kind_glyph(SymbolKind::CONSTANT), 'c');
        assert_eq!(symbol_kind_glyph(SymbolKind::EVENT), '·'); // unmapped kind → fallthrough badge
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_lists_document_symbols() {
        use std::fs;

        // A throwaway cargo project with a struct (+ method) and a function.
        let dir = std::env::temp_dir().join(format!("majestic-ra-ds-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source = "struct Thing;\nimpl Thing {\n    fn method(&self) {}\n}\nfn main() {}\n";
        fs::write(&main_rs, source).unwrap();

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        // Drive startup (`poll`) and keep requesting until the (async-started) server replies.
        let mut symbols = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_document_symbols(&main_rs); // a no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::DocumentSymbols { symbols: found, .. } = outcome {
                    if !found.is_empty() {
                        symbols = found;
                    }
                }
            }
            if !symbols.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        assert!(
            symbols.iter().any(|symbol| symbol.name == "Thing"),
            "should list the struct: {symbols:?}"
        );
        assert!(
            symbols.iter().any(|symbol| symbol.name == "main"),
            "should list main: {symbols:?}"
        );
    }

    #[test]
    fn signature_help_request_yields_active_parameter_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one textDocument/signatureHelp with a two-parameter signature,
        // the second parameter active. Then it idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/signatureHelp" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "signatures": [{
                            "label": "write(buf: &[u8], n: usize)",
                            "parameters": [{"label": "buf: &[u8]"}, {"label": "n: usize"}],
                            "activeParameter": 1
                        }],
                        "activeSignature": 0,
                        "activeParameter": 1
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

        let path = std::path::Path::new("/tmp/does-not-exist-sh.rs");
        manager.open(path, "write(a, b").unwrap();
        manager.request_signature_help(path, "write(a, b".len());

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
        let LspOutcome::SignatureHelp {
            path: got,
            signature,
        } = &outcomes[0]
        else {
            panic!("expected a signature-help outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        // The active parameter "n: usize" occupies bytes 18..26 of the signature label.
        assert_eq!(
            signature,
            &Some(majestic_core::SignatureHelp::new(
                "write(buf: &[u8], n: usize)",
                Some((18, 26))
            ))
        );

        mock.join().unwrap();
    }

    #[test]
    fn parameter_byte_range_resolves_simple_and_offset_labels() {
        use lsp_types::ParameterLabel;
        let label = "f(alpha, beta)";
        // A `Simple` label is located by substring search ("beta" at bytes 9..13).
        assert_eq!(
            parameter_byte_range(label, &ParameterLabel::Simple("beta".to_owned())),
            Some((9, 13))
        );
        // `LabelOffsets` are character offsets into the label → byte offsets ("alpha" at 2..7).
        assert_eq!(
            parameter_byte_range(label, &ParameterLabel::LabelOffsets([2, 7])),
            Some((2, 7))
        );
        // A label that does not occur yields no highlight.
        assert_eq!(
            parameter_byte_range(label, &ParameterLabel::Simple("zzz".to_owned())),
            None
        );
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_offers_signature_help() {
        use std::fs;

        // A throwaway cargo project with a 2-arg function and a call with the cursor after the comma.
        let dir = std::env::temp_dir().join(format!("majestic-ra-sh-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source =
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\nfn main() {\n    let _ = add(1, 2);\n}\n";
        fs::write(&main_rs, source).unwrap();
        // The cursor just after the comma inside `add(1, 2)`.
        let byte = source.find("add(1, ").unwrap() + "add(1, ".len();

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        let mut signature = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_signature_help(&main_rs, byte); // a no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::SignatureHelp {
                    signature: Some(found),
                    ..
                } = outcome
                {
                    signature = Some(found);
                }
            }
            if signature.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        assert!(
            signature.is_some(),
            "rust-analyzer should offer signature help inside the call"
        );
    }

    #[test]
    fn rename_request_yields_edits_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server that answers one textDocument/rename with a `changes` map: two edits in one
        // file (the declaration + a use), both renamed to "bar". Then it idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/rename" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "changes": {
                            "file:///tmp/does-not-exist-rn.rs": [
                                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "bar"},
                                {"range": {"start": {"line": 2, "character": 8}, "end": {"line": 2, "character": 11}}, "newText": "bar"}
                            ]
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

        let path = std::path::Path::new("/tmp/does-not-exist-rn.rs");
        manager.open(path, "fn foo() {}").unwrap();
        manager.request_rename(path, "fn f".len(), "bar".to_owned());

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
        let LspOutcome::Rename { path: got, edits } = &outcomes[0] else {
            panic!("expected a rename outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits[0].path,
            std::path::PathBuf::from("/tmp/does-not-exist-rn.rs")
        );
        assert_eq!(
            (
                edits[0].start_line,
                edits[0].start_character,
                edits[0].end_line,
                edits[0].end_character
            ),
            (0, 3, 0, 6)
        );
        assert_eq!(edits[0].new_text, "bar");
        assert_eq!((edits[1].start_line, edits[1].start_character), (2, 8));

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_renames_a_symbol() {
        use std::fs;

        // A throwaway cargo project where `target` is declared once and called twice.
        let dir = std::env::temp_dir().join(format!("majestic-ra-rn-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source =
            "fn target() -> i32 {\n    42\n}\nfn main() {\n    let _ = target();\n    let _ = target();\n}\n";
        fs::write(&main_rs, source).unwrap();
        let decl_byte = source.find("target").unwrap() + 1;

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        let mut edits = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup (ignore diagnostics)
            manager.request_rename(&main_rs, decl_byte, "renamed".to_owned()); // no-op until ready
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::Rename { edits: found, .. } = outcome {
                    if !found.is_empty() {
                        edits = found;
                    }
                }
            }
            if !edits.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        // The declaration plus the two calls → at least three edits, all in this file, all "renamed".
        assert!(
            edits.len() >= 3,
            "rust-analyzer should rename the declaration and its uses: {edits:?}"
        );
        assert!(edits.iter().all(|edit| edit.path == main_rs));
        assert!(edits.iter().all(|edit| edit.new_text == "renamed"));
    }

    #[test]
    fn document_highlight_request_yields_byte_range_occurrences_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server answering one textDocument/documentHighlight with three ranges for `foo`:
        // the write/definition on line 0 and two reads on line 1. Kinds: 2 = Read, 3 = Write.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/documentHighlight" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 7}}, "kind": 3},
                        {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "kind": 2},
                        {"range": {"start": {"line": 1, "character": 6}, "end": {"line": 1, "character": 9}}, "kind": 2}
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

        let path = std::path::Path::new("/tmp/does-not-exist-dh.rs");
        // The snapshot the positions are converted against (line 0 is 13 bytes incl. the newline).
        manager.open(path, "let foo = 1;\nfoo + foo\n").unwrap();
        manager.request_document_highlight(path, 4);

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
        let LspOutcome::DocumentHighlight {
            path: got,
            occurrences,
        } = &outcomes[0]
        else {
            panic!(
                "expected a document-highlight outcome, got {:?}",
                outcomes[0]
            );
        };
        assert_eq!(got, path);
        assert_eq!(occurrences.len(), 3);
        // Positions map to byte ranges against the snapshot; kind 3 is the write/definition.
        assert_eq!(occurrences[0].range, 4..7);
        assert!(occurrences[0].write);
        assert_eq!(occurrences[1].range, 13..16);
        assert!(!occurrences[1].write);
        assert_eq!(occurrences[2].range, 19..22);

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_highlights_occurrences() {
        use std::fs;

        let dir = std::env::temp_dir().join(format!("majestic-ra-dh-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        let source = "fn main() {\n    let total = 1;\n    let _ = total + total;\n}\n";
        fs::write(&main_rs, source).unwrap();
        // The cursor on the `total` binding.
        let byte = source.find("total").unwrap() + 1;

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        let mut occurrences = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup
            manager.request_document_highlight(&main_rs, byte);
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::DocumentHighlight {
                    occurrences: found, ..
                } = outcome
                {
                    if !found.is_empty() {
                        occurrences = found;
                    }
                }
            }
            if !occurrences.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        // The binding plus its two uses → at least three occurrences, all inside the source.
        assert!(
            occurrences.len() >= 3,
            "rust-analyzer should highlight `total` and its uses: {occurrences:?}"
        );
        assert!(occurrences.iter().all(|occ| occ.range.end <= source.len()));
    }

    #[test]
    fn code_action_request_yields_actions_with_edits_via_poll_outcomes() {
        let (client, server) = UnixStream::pair().unwrap();
        // A mock server answering one textDocument/codeAction with a fix (carrying an edit) and a
        // legacy command (no edit). Then it idles.
        let mock = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let id = loop {
                let message = read_message(&mut reader).unwrap();
                if message["method"] == "textDocument/codeAction" {
                    break message["id"].clone();
                }
            };
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {
                            "title": "Import Write",
                            "kind": "quickfix",
                            "edit": {
                                "changes": {
                                    "file:///tmp/does-not-exist-ca.rs": [
                                        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "use std::io::Write;\n"}
                                    ]
                                }
                            }
                        },
                        {"title": "Run command", "command": "majestic.noop"}
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

        let path = std::path::Path::new("/tmp/does-not-exist-ca.rs");
        manager.open(path, "fn main() {}").unwrap();
        manager.request_code_action(path, 0);

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
        let LspOutcome::CodeActions { path: got, actions } = &outcomes[0] else {
            panic!("expected a code-actions outcome, got {:?}", outcomes[0]);
        };
        assert_eq!(got, path);
        assert_eq!(actions.len(), 2);
        // The CodeAction carries its edit (reduced from the WorkspaceEdit); the Command does not.
        assert_eq!(actions[0].title, "Import Write");
        assert!(actions[0].is_applicable());
        assert_eq!(actions[0].edits.len(), 1);
        assert_eq!(actions[0].edits[0].new_text, "use std::io::Write;\n");
        assert_eq!(
            actions[0].edits[0].path,
            std::path::PathBuf::from("/tmp/does-not-exist-ca.rs")
        );
        assert_eq!(actions[1].title, "Run command");
        assert!(!actions[1].is_applicable()); // command-only

        mock.join().unwrap();
    }

    #[test]
    #[ignore = "spawns real rust-analyzer; run manually, e.g. under `nix-shell -p rust-analyzer`"]
    fn real_rust_analyzer_offers_code_actions() {
        use std::fs;

        let dir = std::env::temp_dir().join(format!("majestic-ra-ca-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        // A struct with fields — rust-analyzer offers assists (generate impl, etc.) on its name.
        let source = "struct Point {\n    x: i32,\n    y: i32,\n}\nfn main() {}\n";
        fs::write(&main_rs, source).unwrap();
        let byte = source.find("Point").unwrap() + 1;

        let mut manager = LspManager::with_defaults();
        manager.open(&main_rs, source).unwrap();

        let mut actions = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let _ = manager.poll(); // advance startup + retain diagnostics
            manager.request_code_action(&main_rs, byte);
            for outcome in manager.poll_outcomes() {
                if let LspOutcome::CodeActions { actions: found, .. } = outcome {
                    if !found.is_empty() {
                        actions = found;
                    }
                }
            }
            if !actions.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let _ = fs::remove_dir_all(&dir);
        assert!(
            !actions.is_empty(),
            "rust-analyzer should offer at least one assist on a struct: {actions:?}"
        );
    }

    #[test]
    fn server_health_reports_the_configured_server_state() {
        let mut manager = LspManager::new();
        // An unconfigured extension has no server.
        assert_eq!(manager.server_health(std::path::Path::new("/x.txt")), None);

        manager.configure("rs", ServerConfig::new("rust-analyzer", "rust"));
        let path = std::path::Path::new("/x.rs");
        // Configured, but no server spawned yet → Starting (with the command name).
        assert!(matches!(
            manager.server_health(path),
            Some((name, ServerHealth::Starting)) if name == "rust-analyzer"
        ));

        // A registered (ready) server → Ready.
        let (client, _server) = UnixStream::pair().unwrap();
        let connection = Connection::new(client.try_clone().unwrap(), client);
        manager.register_server("rust", LanguageServer::from_connection(connection));
        assert!(matches!(
            manager.server_health(path),
            Some((_, ServerHealth::Ready))
        ));
    }
}
