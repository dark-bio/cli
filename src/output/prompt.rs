// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Single-key confirmation input, separated from prompt rendering.

use crate::error::Error;
use console::{Key, Term};
use std::io;

/// Key source that discards queued input before the question is shown.
pub(super) trait Keys {
    /// Discards type-ahead where the platform input API supports it.
    fn discard(&mut self) -> io::Result<()>;
    /// Reads a key without echoing it or waiting for Enter.
    fn read(&mut self) -> io::Result<Key>;
}

/// Console input for an invocation that already permits prompting.
pub(super) struct Input {
    /// Console key reader independent of the prompt's stderr destination.
    terminal: Term,
}

impl Input {
    /// Opens a key reader even when the output streams are redirected.
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        let terminal = {
            use std::os::fd::AsFd;
            // The duplicate supplies terminal detection; this reader never writes
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
            // Stdin remains open during the call, which takes no borrowed buffers
            if unsafe { FlushConsoleInputBuffer(io::stdin().as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // console has no Unix input-flush API, and the direct dependencies
        // expose no termios call; Unix type-ahead remains queued
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

/// Reads Windows console input when both output handles are redirected.
#[cfg(windows)]
fn redirected_key() -> io::Result<Key> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{INPUT_RECORD, KEY_EVENT, ReadConsoleInputW};
    loop {
        // INPUT_RECORD consists only of integers and unions that permit zero
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut count = 0;
        // The input handle stays open, and the call writes one initialized record
        if unsafe { ReadConsoleInputW(io::stdin().as_raw_handle(), &mut record, 1, &mut count) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if count == 0 || record.EventType != KEY_EVENT as u16 {
            continue;
        }
        // EventType selects the KEY_EVENT_RECORD member of the union
        let event = unsafe { record.Event.KeyEvent };
        if event.bKeyDown == 0 {
            continue;
        }
        // Unicode console input initializes the UnicodeChar member
        let character = unsafe { event.uChar.UnicodeChar };
        return Ok(match char::from_u32(u32::from(character)) {
            Some('\r') => Key::Enter,
            Some('\u{3}') => Key::CtrlC,
            Some(ch) => Key::Char(ch),
            None => Key::Unknown,
        });
    }
}

/// Discards queued keys, shows the question, and accepts only y, n or Enter.
///
/// Ctrl-C becomes the interruption error that the caller routes through cleanup.
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

/// Tests using separate queued and newly typed keys, with no terminal or clock.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Answers need one key, ignore other keys and discard type-ahead before display.
    #[test]
    fn test_confirmation_keys() {
        /// Queued input and keys typed after the question appears.
        struct Input {
            /// Keys queued before the prompt, which must never answer it.
            queued: VecDeque<Key>,
            /// Keys available once the prompt is shown.
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
