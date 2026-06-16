// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The single-threaded [`ForegroundExecutor`] and its [`ForegroundSpawner`].
//!
//! Foreground jobs run on the thread that calls [`ForegroundExecutor::run_pending`] — the UI
//! thread. Any thread may schedule work via a [`ForegroundSpawner`] (e.g. a background task
//! posting its result back), and it is drained once per frame: only jobs queued *before* a
//! `run_pending` call run in that call, so a job that schedules more does not starve the
//! frame (run-to-completion, no reentrancy).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use crate::task::{Cancel, Task};

type Job = Box<dyn FnOnce() + Send + 'static>;

fn schedule<F, T>(sender: &Sender<Job>, work: F) -> Task<T>
where
    F: FnOnce(Cancel) -> T + Send + 'static,
    T: Send + 'static,
{
    let cancel = Cancel::new();
    let done = Arc::new(AtomicBool::new(false));
    let (result_tx, result_rx) = channel();
    let job_cancel = cancel.clone();
    let job_done = Arc::clone(&done);
    let job: Job = Box::new(move || {
        let output = work(job_cancel);
        job_done.store(true, Ordering::Release);
        let _ = result_tx.send(output);
    });
    let _ = sender.send(job);
    Task::new(result_rx, cancel, done)
}

/// Runs jobs on the foreground (UI) thread, drained once per frame.
#[derive(Debug)]
pub struct ForegroundExecutor {
    sender: Sender<Job>,
    receiver: Receiver<Job>,
}

impl ForegroundExecutor {
    /// Creates an empty foreground executor.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self { sender, receiver }
    }

    /// Returns a handle other threads can use to schedule foreground work.
    #[must_use]
    pub fn spawner(&self) -> ForegroundSpawner {
        ForegroundSpawner {
            sender: self.sender.clone(),
        }
    }

    /// Schedules `work` to run on the next [`ForegroundExecutor::run_pending`].
    #[must_use = "dropping the Task cancels the work; call .detach() to fire-and-forget"]
    pub fn spawn<F, T>(&self, work: F) -> Task<T>
    where
        F: FnOnce(Cancel) -> T + Send + 'static,
        T: Send + 'static,
    {
        schedule(&self.sender, work)
    }

    /// Runs every job queued before this call on the current thread; returns how many ran.
    ///
    /// Jobs scheduled while draining are deferred to the next call.
    #[must_use = "the return value is how many jobs ran; use `let _ =` to ignore it"]
    pub fn run_pending(&self) -> usize {
        let mut jobs = Vec::new();
        while let Ok(job) = self.receiver.try_recv() {
            jobs.push(job);
        }
        let ran = jobs.len();
        for job in jobs {
            job();
        }
        ran
    }
}

impl Default for ForegroundExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// A cloneable handle for scheduling work onto a [`ForegroundExecutor`] from any thread.
#[derive(Clone, Debug)]
pub struct ForegroundSpawner {
    sender: Sender<Job>,
}

impl ForegroundSpawner {
    /// Schedules `work` to run on the executor's next drain.
    #[must_use = "dropping the Task cancels the work; call .detach() to fire-and-forget"]
    pub fn spawn<F, T>(&self, work: F) -> Task<T>
    where
        F: FnOnce(Cancel) -> T + Send + 'static,
        T: Send + 'static,
    {
        schedule(&self.sender, work)
    }
}

#[cfg(test)]
mod tests {
    use super::ForegroundExecutor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn runs_jobs_on_the_calling_thread() {
        let executor = ForegroundExecutor::new();
        let main = thread::current().id();
        let ran_on = Arc::new(Mutex::new(None));
        let recorder = Arc::clone(&ran_on);

        let task = executor.spawn(move |_| {
            *recorder.lock().unwrap() = Some(thread::current().id());
            7
        });
        assert!(!task.is_finished()); // not run until drained
        assert_eq!(executor.run_pending(), 1);
        assert_eq!(task.join(), Some(7));
        assert_eq!(*ran_on.lock().unwrap(), Some(main));
    }

    #[test]
    fn jobs_scheduled_while_draining_defer_to_next_frame() {
        let executor = ForegroundExecutor::new();
        let count = Arc::new(AtomicUsize::new(0));
        let spawner = executor.spawner();

        let outer_count = Arc::clone(&count);
        let inner_spawner = spawner.clone();
        executor
            .spawn(move |_| {
                outer_count.fetch_add(1, Ordering::SeqCst);
                let inner_count = Arc::clone(&outer_count);
                inner_spawner
                    .spawn(move |_| {
                        inner_count.fetch_add(1, Ordering::SeqCst);
                    })
                    .detach();
            })
            .detach();

        assert_eq!(executor.run_pending(), 1); // only the outer job
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(executor.run_pending(), 1); // the deferred inner job
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
