// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic LSP client — JSON-RPC over a language server's stdio (PRD #1 §6.9).
//!
//! This crate is the transport: the LSP base framing ([`read_message`]/[`write_message`], a
//! `Content-Length` header + JSON body) and a [`Connection`] that correlates request/response
//! pairs and publishes server-initiated messages ([`Incoming`]) to a Morpheus event bus. It is
//! transport-generic, so it drives a real language server's stdio and a mock server over a socket
//! pair in tests. The typed client (the `initialize` handshake, document sync from `Document`
//! revisions, diagnostics → the editor) layers on top of this.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

mod client;
mod codec;
mod connection;
mod manager;

pub use client::{file_uri, LanguageServer};
pub use codec::{read_message, write_message};
pub use connection::{Connection, Incoming, Requester, Response};
pub use manager::{position_to_byte, LspManager, LspOutcome, ServerConfig, ServerHealth};

/// Re-export of `lsp-types` so consumers use exactly the version this client speaks.
pub use lsp_types;
