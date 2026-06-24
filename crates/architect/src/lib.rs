// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic agent loop and natural-language terminal surface (PRD #1 §5.2.3) — the governed AI
//! surface. Provider-agnostic and local-first; every agent side effect passes through Seraph.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! M3 is landing this crate incrementally; the provider abstraction ([`Provider`]) is in place.

mod provider;

#[doc(inline)]
pub use provider::{
    CompletionRequest, CompletionResponse, Message, MockProvider, Provider, ProviderError, Role,
    ToolCall, ToolSpec,
};
