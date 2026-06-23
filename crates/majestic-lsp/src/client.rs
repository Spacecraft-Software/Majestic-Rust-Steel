// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A typed [`LanguageServer`] client over a [`Connection`] (PRD #1 §6.9).
//!
//! Wraps the JSON-RPC connection with the LSP handshake, full-document sync, and diagnostics
//! draining, using `lsp-types` for the payloads. The editor opens a document ([`did_open`]),
//! pushes each revision ([`did_change`]), and drains published diagnostics each frame
//! ([`diagnostics`]). [`spawn`] launches a real server (e.g. rust-analyzer) over its stdio;
//! [`from_connection`] wraps an existing connection, which lets the handshake/sync/diagnostics
//! path be tested against a mock server over a socket pair.
//!
//! [`did_open`]: LanguageServer::did_open
//! [`did_change`]: LanguageServer::did_change
//! [`diagnostics`]: LanguageServer::diagnostics
//! [`spawn`]: LanguageServer::spawn
//! [`from_connection`]: LanguageServer::from_connection

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use lsp_types::{
    ClientCapabilities, CodeActionClientCapabilities, CodeActionKindLiteralSupport,
    CodeActionLiteralSupport, CompletionClientCapabilities, CompletionItemCapability,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingClientCapabilities,
    DocumentHighlightClientCapabilities, DocumentSymbolClientCapabilities, GotoCapability,
    HoverClientCapabilities, InitializeParams, InitializeResult, InitializedParams, MarkupKind,
    PublishDiagnosticsParams, ReferenceClientCapabilities, RenameClientCapabilities,
    SignatureHelpClientCapabilities, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
    WorkspaceClientCapabilities, WorkspaceFolder, WorkspaceSymbolClientCapabilities,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::connection::{Connection, Incoming, Requester};

/// A typed LSP client driving one language server.
#[derive(Debug)]
pub struct LanguageServer {
    connection: Connection,
    child: Option<Child>,
}

impl LanguageServer {
    /// Wraps an existing connection (a socket pair in tests; a child's stdio via [`Self::spawn`]).
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self {
            connection,
            child: None,
        }
    }

    /// Spawns `program args...` and connects to its stdio (stderr is discarded).
    ///
    /// # Errors
    /// Returns an I/O error if the process fails to spawn or its stdio cannot be captured.
    pub fn spawn(program: &str, args: &[&str]) -> io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("language server stdout was not captured"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("language server stdin was not captured"))?;
        Ok(Self {
            connection: Connection::new(stdout, stdin),
            child: Some(child),
        })
    }

    /// A shared handle to the connection's send side, for issuing a request off the editor thread
    /// (the manager hands this to a worker thread for completion/hover). The `!Sync` incoming-bus
    /// stays on the [`Connection`], which the editor keeps draining.
    #[must_use]
    pub fn requester(&self) -> Arc<Requester> {
        self.connection.requester()
    }

    /// Performs the `initialize` handshake for workspace `root` and sends `initialized`.
    ///
    /// # Errors
    /// Returns an I/O error if the exchange fails or the server reports an error.
    pub fn initialize(&self, root: Uri) -> io::Result<InitializeResult> {
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            // `workspace_folders` is the non-deprecated way to point the server at the project.
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "root".to_owned(),
            }]),
            capabilities: client_capabilities(),
            ..Default::default()
        };
        let result = self.request("initialize", params)?;
        self.connection
            .notify("initialized", to_value(InitializedParams {})?)?;
        Ok(result)
    }

    /// Notifies the server a document opened (`textDocument/didOpen`).
    ///
    /// # Errors
    /// Returns an I/O error if the notification cannot be written.
    pub fn did_open(
        &self,
        uri: Uri,
        language_id: &str,
        version: i32,
        text: String,
    ) -> io::Result<()> {
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(uri, language_id.to_owned(), version, text),
        };
        self.connection
            .notify("textDocument/didOpen", to_value(params)?)
    }

    /// Notifies the server a document changed, sending the whole new `text` (full-document sync).
    /// `version` must strictly increase (the editor's `Document` revision drives it).
    ///
    /// # Errors
    /// Returns an I/O error if the notification cannot be written.
    pub fn did_change(&self, uri: Uri, version: i32, text: String) -> io::Result<()> {
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            // No range ⇒ the new text replaces the whole document.
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }],
        };
        self.connection
            .notify("textDocument/didChange", to_value(params)?)
    }

    /// Drains the diagnostics the server has published since the last call, one entry per document
    /// the server reported on. The editor calls this each frame to refresh its problem markers.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<PublishDiagnosticsParams> {
        self.connection
            .drain_incoming()
            .into_iter()
            .filter_map(|message| match message {
                Incoming::Notification { method, params }
                    if method == "textDocument/publishDiagnostics" =>
                {
                    serde_json::from_value(params).ok()
                }
                _ => None,
            })
            .collect()
    }

    /// Requests an orderly shutdown (`shutdown` then `exit`).
    ///
    /// # Errors
    /// Returns an I/O error if the exchange fails or the server reports an error.
    pub fn shutdown(&self) -> io::Result<()> {
        let _: Value = self.request("shutdown", Value::Null)?;
        self.connection.notify("exit", Value::Null)
    }

    /// Sends a typed request and deserializes the typed result.
    fn request<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> io::Result<R> {
        let response = self.connection.request(method, to_value(params)?)?;
        match response.into_result() {
            Ok(result) => serde_json::from_value(result).map_err(io::Error::other),
            Err(error) => Err(io::Error::other(format!("language server error: {error}"))),
        }
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        // A spawned server outlives this handle unless reaped; kill it so no orphan lingers.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The capabilities advertised in the `initialize` handshake: completion, hover, signature help,
/// goto-definition (+ type-definition, implementation), find-references, document highlight, document
/// symbols, code actions, rename, and document formatting, plus the implicit defaults.
/// Completion is requested without snippet support (we insert plain text, not `$0`-style snippet
/// placeholders); hover accepts both Markdown and plain-text content so a server may send whichever
/// it prefers (the editor renders it as text either way); document symbols request the hierarchical
/// (nested) shape so members nest under their parent.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities {
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                ..Default::default()
            }),
            signature_help: Some(SignatureHelpClientCapabilities::default()),
            definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(false),
            }),
            type_definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(false),
            }),
            implementation: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(false),
            }),
            references: Some(ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            document_highlight: Some(DocumentHighlightClientCapabilities {
                dynamic_registration: Some(false),
            }),
            // Advertise code-action *literal* support so the server returns `CodeAction`s (carrying
            // an `edit`) rather than legacy `Command`s. No `resolveSupport`, so rust-analyzer resolves
            // the edits eagerly and we can apply them without a second `codeAction/resolve` round-trip.
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        value_set: vec![
                            "quickfix".to_owned(),
                            "refactor".to_owned(),
                            "source".to_owned(),
                        ],
                    },
                }),
                ..Default::default()
            }),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            rename: Some(RenameClientCapabilities {
                // No `textDocument/prepareRename` round-trip; we send the new name directly.
                prepare_support: Some(false),
                ..Default::default()
            }),
            formatting: Some(DocumentFormattingClientCapabilities {
                dynamic_registration: Some(false),
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            symbol: Some(WorkspaceSymbolClientCapabilities::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Builds a `file://` [`Uri`] for `path`, canonicalizing it when possible.
///
/// # Errors
/// Returns an error if the resulting URI does not parse.
pub fn file_uri(path: &Path) -> io::Result<Uri> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", absolute.display())
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad file URI: {error}"),
            )
        })
}

/// Serializes `value` to a JSON [`Value`], mapping a serialization failure to an I/O error.
fn to_value<T: Serialize>(value: T) -> io::Result<Value> {
    serde_json::to_value(value).map_err(io::Error::other)
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::thread;

    use serde_json::json;

    use super::{file_uri, LanguageServer};
    use crate::codec::{read_message, write_message};
    use crate::connection::Connection;

    #[test]
    fn handshake_syncs_a_document_and_surfaces_typed_diagnostics() {
        let (client, server) = UnixStream::pair().unwrap();

        // A mock server: answer `initialize`, but first push diagnostics so they are already on
        // the bus when `initialize` returns. Then read the follow-up notifications and finish.
        let mock = thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let initialize = read_message(&mut reader).unwrap();
            assert_eq!(initialize["method"], "initialize");
            let id = initialize["id"].clone();
            write_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": "file:///x.rs",
                        "diagnostics": [{
                            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}},
                            "severity": 1,
                            "message": "cannot find value `oops`"
                        }]
                    }
                }),
            )
            .unwrap();
            write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
            )
            .unwrap();
            // Consume the `initialized` notification and the `didOpen` notification, then finish.
            read_message(&mut reader).unwrap(); // initialized
            read_message(&mut reader).unwrap(); // textDocument/didOpen
        });

        let server =
            LanguageServer::from_connection(Connection::new(client.try_clone().unwrap(), client));
        let uri = "file:///x.rs".parse().unwrap();

        // The handshake succeeds and the server reports (empty) capabilities.
        let _result = server.initialize(uri).unwrap();
        server
            .did_open(
                "file:///x.rs".parse().unwrap(),
                "rust",
                1,
                "fn main(){}".to_owned(),
            )
            .unwrap();

        // The diagnostics pushed before the initialize response are already drained as typed data.
        let published = server.diagnostics();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].diagnostics.len(), 1);
        assert_eq!(
            published[0].diagnostics[0].message,
            "cannot find value `oops`"
        );

        mock.join().unwrap();
    }

    #[test]
    fn file_uri_builds_a_file_scheme_uri() {
        let uri = file_uri(std::path::Path::new("/tmp/example.rs")).unwrap();
        assert!(uri.as_str().starts_with("file:///"));
        assert!(uri.as_str().ends_with("example.rs"));
    }
}
