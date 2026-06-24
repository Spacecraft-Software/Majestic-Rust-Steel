// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic AI guardrail and policy engine (PRD #1 §5.2.4) — the mandatory gate every agent side
//! effect passes through: diff-approval, sandboxing, rate limiting, the kill switch, and a
//! tamper-evident audit log. Policy is declarative (Nickel manifest only) and fails closed.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! M3 is landing this crate incrementally: the [`AuditLog`], the fail-closed policy engine
//! ([`Policy::decide`]), and the [`KillSwitch`] are in place.

mod audit;
mod kill_switch;
mod policy;

#[doc(inline)]
pub use audit::{AuditEntry, AuditLog};
#[doc(inline)]
pub use kill_switch::KillSwitch;
#[doc(inline)]
pub use policy::{AgentAction, Decision, Policy};
