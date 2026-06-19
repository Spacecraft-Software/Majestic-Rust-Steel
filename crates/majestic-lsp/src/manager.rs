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
use std::thread::{self, JoinHandle};

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

/// Manages language servers and open-document sync.
#[derive(Debug, Default)]
pub struct LspManager {
    /// Server launch config keyed by file extension (without the dot), e.g. `rs`.
    configs: HashMap<String, ServerConfig>,
    /// Servers keyed by `languageId`, each in its lifecycle state.
    servers: HashMap<String, ServerSlot>,
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
}
