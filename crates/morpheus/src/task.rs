// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Spawnable [`Task`] handles and cooperative [`Cancel`]lation.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// A cooperative cancellation flag shared with spawned work.
///
/// A spawned closure receives a `Cancel` and should poll [`Cancel::is_cancelled`] at natural
/// yield points; when the owning [`Task`] is dropped, the flag is raised.
#[derive(Clone, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Returns `true` once cancellation has been requested (the [`Task`] was dropped).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A handle to spawned work.
///
/// Dropping the handle requests cooperative cancellation and discards the result; call
/// [`Task::detach`] to let the work run untracked, or [`Task::join`] to block for its output.
/// Cancellation is cooperative — a closure that never checks [`Cancel::is_cancelled`] runs to
/// completion regardless, but once the handle is gone its result is dropped.
pub struct Task<T> {
    result: Receiver<T>,
    cancel: Cancel,
    done: Arc<AtomicBool>,
    detached: bool,
}

impl<T> Task<T> {
    pub(crate) fn new(result: Receiver<T>, cancel: Cancel, done: Arc<AtomicBool>) -> Self {
        Self {
            result,
            cancel,
            done,
            detached: false,
        }
    }

    /// Returns `true` once the work has produced its result.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Blocks until the work finishes and returns its output.
    ///
    /// Returns `None` if the work produced no result — it was cancelled before running, or
    /// the executor was shut down. Do not call this on the foreground thread for long work.
    #[must_use]
    pub fn join(self) -> Option<T> {
        self.result.recv().ok()
    }

    /// Lets the work run to completion untracked; it will not be cancelled on drop.
    pub fn detach(mut self) {
        self.detached = true;
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if !self.detached {
            self.cancel.request();
        }
    }
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("finished", &self.is_finished())
            .field("detached", &self.detached)
            .finish_non_exhaustive()
    }
}
