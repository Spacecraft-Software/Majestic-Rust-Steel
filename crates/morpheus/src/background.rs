// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The multi-threaded [`BackgroundExecutor`].

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::task::{Cancel, Task};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Shared worker-pool state: a job queue guarded by a `Mutex` + `Condvar`, plus a shutdown
/// flag. (`std` has no MPMC channel; this is the classic portable pool. `parking_lot` and a
/// lock-free queue are documented post-M0 swaps behind this type.)
struct Pool {
    queue: Mutex<VecDeque<Job>>,
    available: Condvar,
    shutdown: AtomicBool,
}

impl Pool {
    fn push(&self, job: Job) {
        self.lock_queue().push_back(job);
        self.available.notify_one();
    }

    /// Locks the queue, recovering from poisoning so one panicked job can't wedge the pool.
    fn lock_queue(&self) -> std::sync::MutexGuard<'_, VecDeque<Job>> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn worker_loop(&self) {
        loop {
            let mut queue = self.lock_queue();
            let job = loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                queue = self
                    .available
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            };
            drop(queue); // run the job without holding the lock
            job();
        }
    }
}

/// A pool of worker threads that runs CPU work off the foreground thread.
///
/// [`BackgroundExecutor::spawn`] returns a [`Task`] whose drop cancels the work. On drop, the
/// executor drains the queued jobs and joins its workers.
pub struct BackgroundExecutor {
    pool: Arc<Pool>,
    workers: Vec<JoinHandle<()>>,
}

impl BackgroundExecutor {
    /// Creates an executor with one worker per available core.
    #[must_use]
    pub fn new() -> Self {
        Self::with_threads(default_threads())
    }

    /// Creates an executor with `threads` workers (clamped to at least one).
    #[must_use]
    pub fn with_threads(threads: usize) -> Self {
        let threads = threads.max(1);
        let pool = Arc::new(Pool {
            queue: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let workers = (0..threads)
            .map(|_| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || pool.worker_loop())
            })
            .collect::<Vec<_>>();
        Self { pool, workers }
    }

    /// The number of worker threads.
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.workers.len()
    }

    /// Spawns `work` on a worker thread, returning a handle to its result.
    ///
    /// The closure receives a [`Cancel`] it may poll to stop early.
    #[must_use = "dropping the Task cancels the work; call .detach() to fire-and-forget"]
    pub fn spawn<F, T>(&self, work: F) -> Task<T>
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
            let _ = result_tx.send(output); // receiver gone => cancelled/detached; fine
        });
        self.pool.push(job);
        Task::new(result_rx, cancel, done)
    }
}

impl Default for BackgroundExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BackgroundExecutor {
    fn drop(&mut self) {
        self.pool.shutdown.store(true, Ordering::Release);
        self.pool.available.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl fmt::Debug for BackgroundExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundExecutor")
            .field("threads", &self.thread_count())
            .finish()
    }
}

fn default_threads() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

#[cfg(test)]
mod tests {
    use super::BackgroundExecutor;
    use crate::task::Task;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn runs_work_and_returns_results() {
        let executor = BackgroundExecutor::with_threads(4);
        let tasks: Vec<_> = (0..16).map(|i| executor.spawn(move |_| i * i)).collect();
        let sum: i64 = tasks.into_iter().filter_map(Task::join).sum();
        assert_eq!(sum, (0..16).map(|i| i * i).sum());
    }

    #[test]
    fn thread_count_is_clamped() {
        assert_eq!(BackgroundExecutor::with_threads(3).thread_count(), 3);
        assert_eq!(BackgroundExecutor::with_threads(0).thread_count(), 1);
    }

    #[test]
    fn dropping_the_task_requests_cancellation() {
        let executor = BackgroundExecutor::with_threads(2);
        let (started_tx, started_rx) = channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let job_stopped = Arc::clone(&stopped);
        let task = executor.spawn(move |cancel| {
            started_tx.send(()).unwrap();
            while !cancel.is_cancelled() {
                thread::yield_now();
            }
            job_stopped.store(true, Ordering::SeqCst);
        });
        started_rx.recv().unwrap(); // the job is running
        drop(task); // request cancellation
        while !stopped.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        assert!(stopped.load(Ordering::SeqCst));
    }
}
