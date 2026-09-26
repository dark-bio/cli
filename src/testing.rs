// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Test clock gates shared by the command modules' tests.

use darkbio_clock::TestClock;
use std::thread;
use std::time::Instant;

/// Blocks until the earliest wait or timer on the clock is due at `deadline`.
///
/// The advance that reaches the deadline then wakes it, whenever the test makes
/// that advance.
pub(crate) fn wait_deadline(tester: &TestClock, deadline: Instant) {
    while tester.next_deadline() != Some(deadline) {
        thread::yield_now();
    }
}
