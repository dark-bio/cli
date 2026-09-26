// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounds carried by one operation, never stored on a shared client.

use darkbio_clock::Clock;
use std::io;
use std::time::{Duration, Instant};

/// Device approval window with time for relay forwarding and the final reply.
pub(crate) const APPROVAL_WINDOW: Duration = Duration::from_secs(40);
/// Pairing approval window with time for the cloud and device exchanges.
pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(70);

/// Bound on an operation, as an absolute deadline, an inactivity limit, or both.
///
/// Each expected I/O wait gets a fresh inactivity allowance; the absolute
/// deadline never moves. Approval requests use their protocol window instead of
/// the inactivity limit.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Fixed bound shared by every step of an operation, when supplied.
    deadline: Option<Instant>,
    /// Renewed allowance for each expected machine response, when supplied.
    inactivity: Option<Duration>,
}

impl Timing {
    /// Returns the fixed operation bound, for caller-owned authentication.
    pub(crate) fn deadline(self) -> Option<Instant> {
        self.deadline
    }

    /// Bounds a whole operation, or several operations sharing this value.
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            inactivity: None,
        }
    }

    /// Bounds each expected response without limiting the whole operation.
    ///
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

    /// Returns the deadline for the next machine response, measured on the
    /// clock.
    pub(crate) fn io(self, clock: &Clock) -> Instant {
        self.bound(clock, self.inactivity)
    }

    /// Returns the deadline for a request that may wait on an approval.
    ///
    /// The approval window replaces the inactivity allowance and includes a
    /// small allowance for forwarding and replies. A timing with only an
    /// absolute deadline keeps that exact bound.
    pub(crate) fn approval(self, clock: &Clock) -> Instant {
        self.window(clock, APPROVAL_WINDOW)
    }

    /// Returns the deadline for a request with its own protocol wait window,
    /// which replaces the inactivity allowance.
    ///
    /// An absolute-only timing keeps its original deadline.
    pub(crate) fn window(self, clock: &Clock, window: Duration) -> Instant {
        self.bound(clock, self.inactivity.map(|_| window))
    }

    /// Clips a protocol deadline without applying the machine wait allowance.
    pub(crate) fn limit(self, deadline: Instant) -> Instant {
        self.deadline.map_or(deadline, |bound| bound.min(deadline))
    }

    /// Checks the absolute deadline, failing with [`crate::Error::Timeout`] once
    /// it has passed.
    ///
    /// Caller-supplied readers own their per-read timeout, so only a workflow's
    /// absolute deadline can expire while an otherwise active reader runs.
    pub(crate) fn check(self, clock: &Clock) -> Result<(), crate::Error> {
        if self
            .deadline
            .is_some_and(|deadline| clock.now() >= deadline)
        {
            Err(crate::Error::Timeout)
        } else {
            Ok(())
        }
    }

    /// Sleeps on the clock between polls, never past the absolute deadline.
    ///
    /// Poll cadence is independent of the response allowance. A deadline
    /// already reached fails with [`crate::Error::Timeout`] without sleeping.
    pub(crate) fn pause(self, clock: &Clock, interval: Duration) -> Result<(), crate::Error> {
        let wait = match self.deadline {
            Some(deadline) => interval.min(
                deadline
                    .checked_duration_since(clock.now())
                    .filter(|wait| !wait.is_zero())
                    .ok_or(crate::Error::Timeout)?,
            ),
            None => interval,
        };
        clock.sleep(wait);
        Ok(())
    }

    /// Chooses the earlier bound, treating duration overflow as immediate expiry.
    fn bound(self, clock: &Clock, timeout: Option<Duration>) -> Instant {
        let wait = timeout.map(|timeout| {
            let now = clock.now();
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
    /// Uses an absolute deadline without adding an inactivity allowance.
    fn from(deadline: Instant) -> Self {
        Self::until(deadline)
    }
}

/// Time left on a clock before a deadline, as blocking OS calls take it.
pub(crate) trait ClockExt {
    /// Returns the time left before the deadline as a positive OS timeout.
    ///
    /// A passed deadline fails with `TimedOut`, since the standard socket calls
    /// refuse a zero timeout.
    fn remaining(&self, deadline: Instant) -> io::Result<Duration>;
}

impl ClockExt for Clock {
    fn remaining(&self, deadline: Instant) -> io::Result<Duration> {
        deadline
            .checked_duration_since(self.now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }
}

/// Deadline clipping and protocol window regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_clock;

    /// A renewed machine allowance and a longer approval window cannot extend
    /// a caller's absolute workflow deadline.
    #[test]
    fn test_absolute_bound() {
        let clock = test_clock().clock();
        let deadline = clock.now() + Duration::from_secs(1);
        let timing = Timing::inactivity(Duration::from_secs(60)).with_deadline(deadline);
        assert_eq!(timing.io(&clock), deadline);
        assert_eq!(timing.approval(&clock), deadline);
        assert_eq!(Timing::until(deadline).io(&clock), deadline);
    }

    /// Short inactivity limits do not shorten the device's approval window.
    #[test]
    fn test_approval_window() {
        let clock = test_clock().clock();
        let timing = Timing::inactivity(Duration::from_millis(10));
        assert_eq!(timing.io(&clock), clock.now() + Duration::from_millis(10));
        assert!(timing.approval(&clock) > clock.now() + Duration::from_secs(39));
    }
}
