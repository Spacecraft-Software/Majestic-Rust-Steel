// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The bridge between Architect's governed agent loop and the Majestic editor core (PRD #1 §5.2.5).
//!
//! Architect's [`architect::Tools`] trait says *what* tools the agent has and leaves *how* to run
//! them to the host; this crate is that host-side surface. [`BufferTools`] lets the agent read a
//! buffer as hashline-tagged lines and apply tagged edits to it — the concrete I/O the governed loop
//! gates. [`AgentSession`] bundles a turn's configuration for the synchronous path, and [`AgentRunner`]
//! drives a turn on a worker thread — marshaling buffer access and approval back to the UI thread — so
//! the editor stays responsive. It depends on `architect` (the trait) and `majestic-core` (the buffer
//! + hashline ops); the agent loop, Seraph, and the UI sit above it.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

mod runner;
mod session;
mod tools;

#[doc(inline)]
pub use runner::{AgentEvent, AgentRunner};
#[doc(inline)]
pub use session::AgentSession;
#[doc(inline)]
pub use tools::{buffer_tool_specs, BufferTools};
