// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Reads the one key that answers a yes or no prompt.
//!
//! Keys come through [`Keys`], so tests type them without a terminal.

use crate::error::Error;
use console::{Key, Term};
use darkbio_clock::Clock;
use std::io;
use std::time::Duration;

/// Time after the question is flushed when keys count as type-ahead, since
/// a person cannot read and answer a question within 100ms.
const TYPEAHEAD_WINDOW: Duration = Duration::from_millis(100);

/// Source of the keys that answer a prompt.
pub(super) trait Keys {
    /// Drops the keys typed before the prompt, where the platform allows it.
    fn discard(&mut self) -> io::Result<()>;
    /// Reads a key without echoing it or waiting for Enter.
    fn read(&mut self) -> io::Result<Key>;
}

/// The terminal's keys, for an invocation that may prompt.
pub(super) struct Input {
    /// Reader on the terminal, whichever stream the prompt prints on.
    terminal: Term,
}

impl Input {
    /// Opens the terminal for reading, even when stdout and stderr are
    /// redirected.
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        let terminal = {
            use std::os::fd::AsFd;
            // console checks its write side for a terminal, so a duplicate of
            // stdin stands in for it, and nothing writes to it
            let descriptor = io::stdin().as_fd().try_clone_to_owned()?;
            Term::read_write_pair(io::stdin(), std::fs::File::from(descriptor))
        };
        #[cfg(not(unix))]
        let terminal = if Term::stderr().is_term() {
            Term::stderr()
        } else {
            Term::stdout()
        };
        Ok(Self { terminal })
    }
}

impl Keys for Input {
    fn discard(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Console::FlushConsoleInputBuffer;
            // Sound since stdin's handle stays open for the call, which takes
            // no buffers
            if unsafe { FlushConsoleInputBuffer(io::stdin().as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // On Unix the early keys stay queued for the timed filter below,
        // since no direct dependency can flush terminal input
        Ok(())
    }

    fn read(&mut self) -> io::Result<Key> {
        #[cfg(windows)]
        if !self.terminal.is_term() {
            return redirected_key();
        }
        self.terminal.read_key_raw()
    }
}

/// Reads a key from the Windows console input when stdout and stderr are both
/// redirected.
#[cfg(windows)]
fn redirected_key() -> io::Result<Key> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{INPUT_RECORD, KEY_EVENT, ReadConsoleInputW};
    loop {
        // Sound since INPUT_RECORD holds only integers and unions, for which
        // zero is a valid value
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut count = 0;
        // Sound since stdin's handle stays open, and the call fills at most
        // the one record it gets
        if unsafe { ReadConsoleInputW(io::stdin().as_raw_handle(), &mut record, 1, &mut count) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if count == 0 || record.EventType != KEY_EVENT as u16 {
            continue;
        }
        // Sound since a key event's type marks the union as a key record
        let event = unsafe { record.Event.KeyEvent };
        if event.bKeyDown == 0 {
            continue;
        }
        // Sound since the wide-character read fills the UnicodeChar member
        let character = unsafe { event.uChar.UnicodeChar };
        return Ok(match char::from_u32(u32::from(character)) {
            Some('\r') => Key::Enter,
            Some('\u{3}') => Key::CtrlC,
            Some(ch) => Key::Char(ch),
            None => Key::Unknown,
        });
    }
}

/// Shows the question and returns the first answer after the type-ahead window.
///
/// `show` flushes the question before it returns, and the window starts then,
/// on `clock`. Keys read within the window are discarded. After it, `y` and
/// `n` answer, Enter takes `default`, and other keys are ignored. Ctrl-C
/// returns the interruption error for the caller's cleanup.
pub(super) fn confirm(
    input: &mut impl Keys,
    clock: &Clock,
    default: bool,
    show: impl FnOnce() -> Result<(), Error>,
    echo: impl FnOnce(bool) -> Result<(), Error>,
) -> Result<bool, Error> {
    // Flush what the platform can, then time type-ahead from the shown question
    input.discard()?;
    show()?;
    let ready = clock.now() + TYPEAHEAD_WINDOW;

    // Drain every early key before accepting an answer, while Ctrl-C always
    // interrupts
    let answer = loop {
        let key = input.read()?;
        if key == Key::CtrlC {
            return Err(Error::new(130, "interrupted", "command interrupted"));
        }
        if clock.now() < ready {
            continue;
        }
        match key {
            Key::Char('y' | 'Y') => break true,
            Key::Char('n' | 'N') => break false,
            Key::Enter => break default,
            _ => {}
        }
    };

    // Leave the accepted answer on the prompt's line
    echo(answer)?;
    Ok(answer)
}

/// Tests of the answer keys, typed without a terminal.
#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_clock::TestClock;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Instant;

    /// Keys with arrival times on a clock the test controls.
    struct Input<'a> {
        /// Clock advanced when a key arrives or the question is shown.
        tester: &'a RefCell<TestClock>,
        /// Keys in arrival order, including type-ahead.
        typed: VecDeque<(Instant, Key)>,
    }

    impl Keys for Input<'_> {
        fn discard(&mut self) -> io::Result<()> {
            // Leave type-ahead queued, as on a terminal without a buffer flush
            Ok(())
        }

        fn read(&mut self) -> io::Result<Key> {
            let (arrived, key) = self.typed.pop_front().expect("test provides an answer");
            self.tester.borrow_mut().advance_to(arrived);
            Ok(key)
        }
    }

    /// Checks that one key answers and other keys are ignored after the window.
    #[test]
    fn test_confirmation_keys() {
        for (case, keys, default, expected) in [
            ("yes", vec![Key::Char('y')], false, true),
            ("uppercase yes", vec![Key::Char('Y')], false, true),
            ("no", vec![Key::Char('n')], true, false),
            ("uppercase no", vec![Key::Char('N')], true, false),
            ("default yes", vec![Key::Enter], true, true),
            ("default no", vec![Key::Enter], false, false),
            (
                "ignored keys",
                vec![Key::ArrowLeft, Key::Char('x'), Key::Char('y')],
                false,
                true,
            ),
        ] {
            let tester = RefCell::new(TestClock::new());
            let clock = tester.borrow().clock();
            let arrived = clock.now() + Duration::from_millis(250);
            let mut input = Input {
                tester: &tester,
                typed: keys.into_iter().map(|key| (arrived, key)).collect(),
            };
            let mut echoed = None;
            assert_eq!(
                confirm(
                    &mut input,
                    &clock,
                    default,
                    || Ok(()),
                    |answer| {
                        echoed = Some(answer);
                        Ok(())
                    }
                )
                .unwrap(),
                expected,
                "{case}"
            );
            assert_eq!(echoed, Some(expected), "{case}");
        }
    }

    /// Drops every early key and starts the window only once the question shows.
    #[test]
    fn test_confirmation_discards_typeahead() {
        for (case, show_delay, keys, expected) in [
            (
                "queued burst",
                0,
                vec![
                    (0, Key::Char('y')),
                    (0, Key::Char('n')),
                    (0, Key::Enter),
                    (0, Key::Char('y')),
                    (250, Key::Char('y')),
                ],
                true,
            ),
            (
                "window boundary",
                0,
                vec![
                    (1, Key::Char('y')),
                    (25, Key::Char('n')),
                    (50, Key::Enter),
                    (75, Key::ArrowLeft),
                    (99, Key::Char('Y')),
                    (100, Key::Char('n')),
                ],
                false,
            ),
            (
                "slow question",
                1000,
                vec![(0, Key::Char('y')), (99, Key::Enter), (250, Key::Char('n'))],
                false,
            ),
        ] {
            // Schedule arrivals relative to the question's flush, even if it waits
            let tester = RefCell::new(TestClock::new());
            let clock = tester.borrow().clock();
            let shown = clock.now() + Duration::from_millis(show_delay);
            let mut input = Input {
                tester: &tester,
                typed: keys
                    .into_iter()
                    .map(|(ms, key)| (shown + Duration::from_millis(ms), key))
                    .collect(),
            };

            // Only the final key may answer, regardless of the earlier burst
            let mut echoed = None;
            assert_eq!(
                confirm(
                    &mut input,
                    &clock,
                    false,
                    || {
                        tester.borrow_mut().advance_to(shown);
                        Ok(())
                    },
                    |answer| {
                        echoed = Some(answer);
                        Ok(())
                    }
                )
                .unwrap(),
                expected,
                "{case}"
            );
            assert_eq!(echoed, Some(expected), "{case}");
            assert!(input.typed.is_empty(), "{case}");
        }
    }

    /// Checks that Ctrl-C interrupts the question inside the type-ahead window
    /// and after it.
    #[test]
    fn test_confirmation_interruption() {
        for arrival in [25, 250] {
            let tester = RefCell::new(TestClock::new());
            let clock = tester.borrow().clock();
            let mut input = Input {
                tester: &tester,
                typed: vec![(clock.now() + Duration::from_millis(arrival), Key::CtrlC)].into(),
            };
            let error = confirm(
                &mut input,
                &clock,
                false,
                || Ok(()),
                |_| panic!("interruption has no answer"),
            )
            .unwrap_err();
            assert_eq!(error.class, 130, "{arrival}ms");
        }
    }
}
