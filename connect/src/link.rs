// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! The close signal shared by the two directions of a transport, and the
//! deadline check the reads of both consult.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Close signal shared by the two directions of a transport. The reads of a
/// transport wait in rounds and consult it between rounds, as a blocked
/// transfer cannot be woken up directly.
pub(crate) struct Link {
    closing: AtomicBool, // Whether the connection was closed, ending the reads
}

impl Link {
    /// Creates the signal of an open transport.
    pub fn new() -> Self {
        Self {
            closing: AtomicBool::new(false),
        }
    }

    /// Marks the connection closed, the reads ending on their next round.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Whether the connection was closed.
    pub fn closed(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
}

/// Whether a deadline is set and has passed.
pub(crate) fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}
