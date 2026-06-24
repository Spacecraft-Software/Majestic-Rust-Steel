// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The agent-stop-all kill switch (PRD #1 §5.2.4) — the user's panic button for the agent.
//!
//! A [`KillSwitch`] is a cheap, cloneable, thread-safe stop signal. The user engages it from any
//! keymap profile, and the agent loop polls [`KillSwitch::is_engaged`] at every step — before each
//! tool call and on each streamed token — and aborts the instant it is set. Because engaging is a
//! single atomic store, the signal propagates far inside the ≤100 ms budget; the only latency is how
//! often the loop checks, which is every step. This is the *cooperative* half of stopping the agent;
//! the host pairs it with dropping the agent's `morpheus::Task` to cancel any in-flight I/O actively.
//!
//! It is deliberately distinct from `morpheus::Cancel`, whose trigger is task-drop only (private to
//! morpheus): the kill switch is **user-triggerable** and shared across a whole agent session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared, user-triggerable stop signal for the agent. Clones share one flag, so the UI thread can
/// engage a switch the agent loop is polling.
#[derive(Clone, Debug, Default)]
pub struct KillSwitch {
    engaged: Arc<AtomicBool>,
}

impl KillSwitch {
    /// Creates a fresh switch in the *not-engaged* state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Engages the switch — every clone's [`Self::is_engaged`] now returns `true`. Idempotent; a
    /// single relaxed atomic store, so it returns immediately.
    pub fn engage(&self) {
        // Relaxed is sufficient: the flag guards no companion data, only its own coherent value.
        self.engaged.store(true, Ordering::Relaxed);
    }

    /// Whether the switch has been engaged. The agent loop checks this between steps.
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
    }

    /// Resets the switch to *not-engaged*, readying it for a new agent session.
    pub fn reset(&self) {
        self.engaged.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::KillSwitch;

    #[test]
    fn starts_disengaged_and_engages() {
        let switch = KillSwitch::new();
        assert!(!switch.is_engaged());
        switch.engage();
        assert!(switch.is_engaged());
    }

    #[test]
    fn clones_share_one_flag() {
        // The agent loop holds a clone of the switch the UI engages.
        let ui = KillSwitch::new();
        let agent = ui.clone();
        ui.engage();
        assert!(agent.is_engaged(), "a clone must observe the engage");
    }

    #[test]
    fn engage_is_idempotent_and_resettable() {
        let switch = KillSwitch::new();
        switch.engage();
        switch.engage(); // idempotent
        assert!(switch.is_engaged());
        switch.reset();
        assert!(!switch.is_engaged(), "reset readies it for a new session");
    }

    #[test]
    fn engaging_from_another_thread_is_observed() {
        // Proves the switch is Send + Sync and the store is visible across threads.
        let switch = KillSwitch::new();
        let remote = switch.clone();
        std::thread::spawn(move || remote.engage())
            .join()
            .expect("thread joins");
        assert!(switch.is_engaged());
    }
}
