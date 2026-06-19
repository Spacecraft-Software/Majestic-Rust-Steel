// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`LspManager`]: spawn/reuse a language server per language, sync open documents, and route
//! published diagnostics back to the editor as byte-range [`Diagnostic`]s (PRD #1 §6.9).
//!
//! One server runs per `languageId` (shared by every open document of that language). The host
//! calls [`LspManager::open`] when a file is shown, [`LspManager::change`] on each edit, and
//! [`LspManager::poll`] each frame to collect diagnostics — converting the server's
//! line/character positions to byte offsets against the document's current text.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, PublishDiagnosticsParams, Uri};
use majestic_core::{Diagnostic, Severity};

use crate::client::{file_uri, LanguageServer};

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
}

/// Manages language servers and open-document sync.
#[derive(Debug, Default)]
pub struct LspManager {
    /// Server launch config keyed by file extension (without the dot), e.g. `rs`.
    configs: HashMap<String, ServerConfig>,
    /// Running servers keyed by `languageId`.
    servers: HashMap<String, LanguageServer>,
    /// Open documents keyed by path.
    docs: HashMap<PathBuf, DocState>,
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

    /// Registers an already-connected server for `language_id` (tests inject a mock this way,
    /// bypassing process spawning).
    pub fn register_server(&mut self, language_id: &str, server: LanguageServer) {
        self.servers.insert(language_id.to_owned(), server);
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
        self.ensure_server(path, &config)?;
        let language_id = config.language_id;
        if let Some(server) = self.servers.get(&language_id) {
            server.did_open(file_uri(path)?, &language_id, 1, text.to_owned())?;
        }
        self.docs.insert(
            path.to_owned(),
            DocState {
                language_id,
                version: 1,
                text: text.to_owned(),
            },
        );
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
        let (version, language_id) = (doc.version, doc.language_id.clone());
        if let Some(server) = self.servers.get(&language_id) {
            server.did_change(file_uri(path)?, version, text.to_owned())?;
        }
        Ok(())
    }

    /// Drains every server's published diagnostics, returning them per document as byte-range
    /// [`Diagnostic`]s (converted against the document's current text). The host applies each list
    /// to the matching editor. Non-blocking.
    #[must_use]
    pub fn poll(&self) -> Vec<(PathBuf, Vec<Diagnostic>)> {
        let mut updates = Vec::new();
        for server in self.servers.values() {
            for published in server.diagnostics() {
                if let Some(update) = self.convert(&published) {
                    updates.push(update);
                }
            }
        }
        updates
    }

    fn convert(&self, published: &PublishDiagnosticsParams) -> Option<(PathBuf, Vec<Diagnostic>)> {
        let path = uri_to_path(&published.uri)?;
        let text = self.docs.get(&path).map_or("", |doc| doc.text.as_str());
        let diagnostics = published
            .diagnostics
            .iter()
            .map(|diagnostic| to_core_diagnostic(text, diagnostic))
            .collect();
        Some((path, diagnostics))
    }

    fn config_for(&self, path: &Path) -> Option<ServerConfig> {
        let extension = path.extension()?.to_str()?;
        self.configs.get(extension).cloned()
    }

    fn ensure_server(&mut self, path: &Path, config: &ServerConfig) -> io::Result<()> {
        if self.servers.contains_key(&config.language_id) {
            return Ok(());
        }
        let args: Vec<&str> = config.args.iter().map(String::as_str).collect();
        let server = LanguageServer::spawn(&config.command, &args)?;
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        server.initialize(file_uri(root)?)?;
        self.servers.insert(config.language_id.clone(), server);
        Ok(())
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
fn position_to_byte(text: &str, line: u32, character: u32) -> usize {
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

/// Best-effort `file://` URI → filesystem path (v1 assumes no percent-encoding).
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    uri.as_str().strip_prefix("file://").map(PathBuf::from)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{position_to_byte, LspManager, ServerConfig};
    use crate::client::LanguageServer;
    use crate::codec::write_message;
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
}
