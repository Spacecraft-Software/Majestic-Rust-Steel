// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A seedable, single-threaded [`DeterministicExecutor`] for testing concurrent logic.
//!
//! Real threads interleave unpredictably, which makes concurrency bugs hard to reproduce.
//! This executor runs spawned jobs one at a time on the calling thread, picking the next
//! ready job by a seeded PRNG — so a given seed always produces the same interleaving, and a
//! failing schedule reproduces from its seed. Jobs may spawn further jobs (modelling
//! message-passing steps), and [`DeterministicExecutor::run`] drains them all.
//!
//! Shipping a Morpheus concurrency feature without a deterministic test is a review blocker;
//! this is the harness those tests drive. It is single-threaded by construction (`!Send`).
//!
//! # Examples
//! ```
//! use std::cell::RefCell;
//! use std::rc::Rc;
//! use morpheus::DeterministicExecutor;
//!
//! let mut executor = DeterministicExecutor::new(0xC0FFEE);
//! let log = Rc::new(RefCell::new(Vec::new()));
//! for i in 0..5 {
//!     let log = Rc::clone(&log);
//!     executor.spawn(move || log.borrow_mut().push(i));
//! }
//! assert_eq!(executor.run(), 5);
//! let mut seen = Rc::try_unwrap(log).unwrap().into_inner();
//! seen.sort_unstable();
//! assert_eq!(seen, [0, 1, 2, 3, 4]); // every job ran, in a seeded order
//! ```

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

type Job = Box<dyn FnOnce()>;

#[derive(Default)]
struct Shared {
    queue: Vec<Job>,
}

/// A single-threaded executor that runs spawned jobs in a seed-determined order.
pub struct DeterministicExecutor {
    shared: Rc<RefCell<Shared>>,
    rng: Rng,
}

impl DeterministicExecutor {
    /// Creates an executor whose scheduling order is determined by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            shared: Rc::new(RefCell::new(Shared::default())),
            rng: Rng::new(seed),
        }
    }

    /// Returns a cloneable handle for spawning jobs (including from within a job).
    #[must_use]
    pub fn spawner(&self) -> DeterministicSpawner {
        DeterministicSpawner {
            shared: Rc::clone(&self.shared),
        }
    }

    /// Spawns `job` onto the executor.
    pub fn spawn(&self, job: impl FnOnce() + 'static) {
        self.shared.borrow_mut().queue.push(Box::new(job));
    }

    /// Runs all jobs (and any they spawn) in seeded order until none remain.
    ///
    /// Returns how many jobs ran. The same seed yields the same interleaving.
    pub fn run(&mut self) -> usize {
        let mut ran = 0;
        loop {
            let job = {
                let mut shared = self.shared.borrow_mut();
                if shared.queue.is_empty() {
                    break;
                }
                let index = self.rng.below(shared.queue.len());
                shared.queue.swap_remove(index)
            };
            job();
            ran += 1;
        }
        ran
    }
}

impl fmt::Debug for DeterministicExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeterministicExecutor")
            .finish_non_exhaustive()
    }
}

/// A cloneable handle for spawning jobs onto a [`DeterministicExecutor`].
#[derive(Clone)]
pub struct DeterministicSpawner {
    shared: Rc<RefCell<Shared>>,
}

impl DeterministicSpawner {
    /// Spawns `job` onto the executor.
    pub fn spawn(&self, job: impl FnOnce() + 'static) {
        self.shared.borrow_mut().queue.push(Box::new(job));
    }
}

impl fmt::Debug for DeterministicSpawner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeterministicSpawner")
            .finish_non_exhaustive()
    }
}

/// Tiny deterministic PRNG (xorshift64*).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1)) // a zero state would be a fixed point
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        usize::try_from(x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::DeterministicExecutor;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn run_order(seed: u64) -> Vec<u32> {
        let mut executor = DeterministicExecutor::new(seed);
        let order = Rc::new(RefCell::new(Vec::new()));
        for i in 0..20u32 {
            let order = Rc::clone(&order);
            executor.spawn(move || order.borrow_mut().push(i));
        }
        assert_eq!(executor.run(), 20);
        Rc::try_unwrap(order).unwrap().into_inner()
    }

    #[test]
    fn same_seed_is_reproducible_and_runs_every_job() {
        let first = run_order(0xABCD);
        let second = run_order(0xABCD);
        assert_eq!(first, second, "same seed must reproduce the schedule");

        let mut sorted = first;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..20).collect::<Vec<_>>(),
            "every job ran exactly once"
        );
    }

    #[test]
    fn jobs_can_spawn_more_jobs() {
        let mut executor = DeterministicExecutor::new(7);
        let spawner = executor.spawner();
        let count = Rc::new(RefCell::new(0u32));

        let outer = Rc::clone(&count);
        executor.spawn(move || {
            *outer.borrow_mut() += 1;
            let inner = Rc::clone(&outer);
            spawner.spawn(move || *inner.borrow_mut() += 1);
        });

        assert_eq!(executor.run(), 2); // the spawned-from-within job ran too
        assert_eq!(*count.borrow(), 2);
    }
}
