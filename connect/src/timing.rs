// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounds carried by one operation, never stored on a shared client.

use std::time::{Duration, Instant};

pub(crate) const APPROVAL_WINDOW: Duration = Duration::from_secs(40);
pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(70);

/// An absolute deadline, an inactivity limit, or both. Each expected I/O wait
/// gets a fresh inactivity allowance; the absolute deadline never moves.
/// Approval requests use their protocol window instead of the inactivity limit.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    deadline: Option<Instant>,
    inactivity: Option<Duration>,
}

impl Timing {
    /// Bounds a whole operation, or several operations sharing this value.
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            inactivity: None,
        }
    }

    /// Bounds each expected response without limiting the whole operation.
    /// Readers supplied by the caller must impose their own read timeout.
    pub fn inactivity(timeout: Duration) -> Self {
        Self {
            deadline: None,
            inactivity: Some(timeout),
        }
    }

    /// Adds an absolute bound without changing the inactivity allowance.
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Deadline for the next machine response.
    pub(crate) fn io(self) -> Instant {
        self.bound(self.inactivity)
    }

    /// Approval windows include a small allowance for forwarding and replies.
    /// Callers using only an absolute deadline retain that exact bound.
    pub(crate) fn approval(self) -> Instant {
        self.window(APPROVAL_WINDOW)
    }

    pub(crate) fn window(self, window: Duration) -> Instant {
        self.bound(self.inactivity.map(|_| window))
    }

    /// Clips a protocol deadline without applying the machine wait allowance.
    pub(crate) fn limit(self, deadline: Instant) -> Instant {
        self.deadline.map_or(deadline, |bound| bound.min(deadline))
    }

    /// Caller-supplied readers own their per-read timeout. Only a workflow's
    /// absolute deadline can expire while an otherwise active reader runs.
    pub(crate) fn check(self) -> Result<(), crate::Error> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Err(crate::Error::Timeout)
        } else {
            Ok(())
        }
    }

    /// Poll cadence is independent of the response allowance.
    pub(crate) fn pause(self, interval: Duration) -> Result<(), crate::Error> {
        let wait = match self.deadline {
            Some(deadline) => interval.min(
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|wait| !wait.is_zero())
                    .ok_or(crate::Error::Timeout)?,
            ),
            None => interval,
        };
        std::thread::sleep(wait);
        Ok(())
    }

    fn bound(self, timeout: Option<Duration>) -> Instant {
        let wait = timeout.map(|timeout| {
            let now = Instant::now();
            now.checked_add(timeout).unwrap_or(now)
        });
        match (self.deadline, wait) {
            (Some(deadline), Some(wait)) => deadline.min(wait),
            (Some(deadline), None) => deadline,
            (None, Some(wait)) => wait,
            (None, None) => unreachable!("timing always carries a bound"),
        }
    }
}

impl From<Instant> for Timing {
    fn from(deadline: Instant) -> Self {
        Self::until(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A renewed machine allowance and a longer approval window cannot extend
    /// a caller's absolute workflow deadline.
    #[test]
    fn test_absolute_bound() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let timing = Timing::inactivity(Duration::from_secs(60)).with_deadline(deadline);
        assert_eq!(timing.io(), deadline);
        assert_eq!(timing.approval(), deadline);
        assert_eq!(Timing::until(deadline).io(), deadline);
    }

    /// Short inactivity limits do not shorten the device's approval window.
    #[test]
    fn test_approval_window() {
        let timing = Timing::inactivity(Duration::from_millis(10));
        assert!(timing.io() < Instant::now() + Duration::from_secs(1));
        assert!(timing.approval() > Instant::now() + Duration::from_secs(39));
    }
}
