// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Morpheus — Majestic's concurrency core (PRD #1 §6.4).
//!
//! Concurrency is designed in from the start (Standard §3.2). Morpheus splits work across
//! two executors so the rule "the main thread is holy" holds by construction:
//!
//! - [`ForegroundExecutor`] runs jobs on the UI thread, drained once per frame — input
//!   dispatch, state mutation, and frame composition live here and never block.
//! - [`BackgroundExecutor`] runs CPU work (search, highlighting, agent context assembly) on
//!   a worker pool sized to the machine's cores.
//!
//! [`spawn`](BackgroundExecutor::spawn) returns a [`Task`] whose drop cooperatively cancels
//! the work (the snapshot ping-pong pattern: hand a background thread an immutable
//! [`stratum::Rope`](https://Majestic.SpacecraftSoftware.org/) snapshot, stream results back,
//! and dropping the receiver cancels). An [`EventBus`] carries subsystem events to a single
//! drain point per frame, and a seedable [`DeterministicExecutor`] makes concurrent logic
//! reproducibly testable.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # M0 substrate note
//! This is the `std::thread` implementation, behind a swap-ready surface. The PRD names
//! `smol` / `async-task` for the executors and `parking_lot` for locks; both slot in behind
//! these types later (the public API — `spawn`, [`Task`], `run_pending`, [`EventBus`] — is
//! the contract, not the threading mechanism).

mod background;
mod deterministic;
mod event_bus;
mod foreground;
mod task;

#[doc(inline)]
pub use background::BackgroundExecutor;
#[doc(inline)]
pub use deterministic::{DeterministicExecutor, DeterministicSpawner};
#[doc(inline)]
pub use event_bus::{Emitter, EventBus};
#[doc(inline)]
pub use foreground::{ForegroundExecutor, ForegroundSpawner};
#[doc(inline)]
pub use task::{Cancel, Task};
