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
use std::io;

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
        // On Unix the early keys stay queued, since neither console nor any
        // other direct dependency can flush terminal input
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

/// Shows the question once type-ahead is dropped, and returns the answer of
/// the first key that gives one.
///
/// `y` and `n` answer, Enter takes `default`, and other keys are ignored.
/// Ctrl-C returns the interruption error, which the caller routes through
/// cleanup.
pub(super) fn confirm(
    input: &mut impl Keys,
    default: bool,
    show: impl FnOnce() -> Result<(), Error>,
    echo: impl FnOnce(bool) -> Result<(), Error>,
) -> Result<bool, Error> {
    input.discard()?;
    show()?;
    let answer = loop {
        match input.read()? {
            Key::Char('y' | 'Y') => break true,
            Key::Char('n' | 'N') => break false,
            Key::Enter => break default,
            Key::CtrlC => return Err(Error::new(130, "interrupted", "command interrupted")),
            _ => {}
        }
    };
    echo(answer)?;
    Ok(answer)
}

/// Tests of the answer keys, typed without a terminal.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Checks that one key answers, other keys are ignored and type-ahead
    /// never answers.
    #[test]
    fn test_confirmation_keys() {
        /// Keys typed before the question and after it.
        struct Input {
            /// Keys typed before the question, which must never answer it.
            queued: VecDeque<Key>,
            /// Keys typed once the question shows.
            typed: VecDeque<Key>,
        }
        impl Keys for Input {
            fn discard(&mut self) -> io::Result<()> {
                self.queued.clear();
                Ok(())
            }
            fn read(&mut self) -> io::Result<Key> {
                Ok(self
                    .queued
                    .pop_front()
                    .or_else(|| self.typed.pop_front())
                    .expect("test provides an answer"))
            }
        }
        for (keys, default, expected) in [
            (vec![Key::Char('y')], false, true),
            (vec![Key::Char('n')], true, false),
            (vec![Key::Enter], true, true),
            (vec![Key::Enter], false, false),
            (
                vec![Key::ArrowLeft, Key::Char('x'), Key::Char('y')],
                false,
                true,
            ),
        ] {
            let mut input = Input {
                queued: vec![Key::Char('n'), Key::Enter].into(),
                typed: keys.into(),
            };
            let mut echoed = None;
            assert_eq!(
                confirm(
                    &mut input,
                    default,
                    || Ok(()),
                    |answer| {
                        echoed = Some(answer);
                        Ok(())
                    }
                )
                .unwrap(),
                expected
            );
            assert_eq!(echoed, Some(expected));
        }
        let mut input = Input {
            queued: VecDeque::new(),
            typed: vec![Key::CtrlC].into(),
        };
        let error = confirm(
            &mut input,
            false,
            || Ok(()),
            |_| panic!("interruption has no answer"),
        )
        .unwrap_err();
        assert_eq!(error.class, 130);
    }
}
