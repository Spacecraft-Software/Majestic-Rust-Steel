// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A single-drain [`EventBus`] for per-frame event processing.
//!
//! Subsystems (PTY output, buffer-changed notifications, agent events) post events through
//! cloneable [`Emitter`]s from any thread; the foreground loop calls [`EventBus::drain`]
//! once per frame and processes the batch run-to-completion. Draining a snapshot (rather
//! than looping until empty) keeps a burst of events from starving a frame.

use std::sync::mpsc::{channel, Receiver, Sender};

/// A queue of events drained once per frame on the foreground thread.
#[derive(Debug)]
pub struct EventBus<E> {
    sender: Sender<E>,
    receiver: Receiver<E>,
}

impl<E> EventBus<E> {
    /// Creates an empty event bus.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self { sender, receiver }
    }

    /// Returns a cloneable emitter that any thread can post events through.
    #[must_use]
    pub fn emitter(&self) -> Emitter<E> {
        Emitter {
            sender: self.sender.clone(),
        }
    }

    /// Removes and returns every event queued before this call, in arrival order.
    #[must_use = "draining and discarding the events loses them"]
    pub fn drain(&self) -> Vec<E> {
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            events.push(event);
        }
        events
    }
}

impl<E> Default for EventBus<E> {
    fn default() -> Self {
        Self::new()
    }
}

/// A cloneable handle for posting events onto an [`EventBus`].
#[derive(Clone, Debug)]
pub struct Emitter<E> {
    sender: Sender<E>,
}

impl<E> Emitter<E> {
    /// Posts `event`, returning `false` if the bus has been dropped.
    pub fn emit(&self, event: E) -> bool {
        self.sender.send(event).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::EventBus;
    use std::thread;

    #[test]
    fn collects_events_from_multiple_emitters() {
        let bus: EventBus<u32> = EventBus::new();
        let from_main = bus.emitter();
        let from_thread = bus.emitter();

        let handle = thread::spawn(move || {
            for i in 0..50 {
                assert!(from_thread.emit(i));
            }
        });
        for i in 100..150 {
            assert!(from_main.emit(i));
        }
        handle.join().unwrap();

        let mut events = bus.drain();
        events.sort_unstable();
        assert_eq!(events.len(), 100);
        assert_eq!(events.first(), Some(&0));
        assert_eq!(events.last(), Some(&149));
        // Drained once: a second drain is empty.
        assert!(bus.drain().is_empty());
    }

    #[test]
    fn emit_after_bus_dropped_is_false() {
        let bus: EventBus<u8> = EventBus::new();
        let emitter = bus.emitter();
        drop(bus);
        assert!(!emitter.emit(1));
    }
}
