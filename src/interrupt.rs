// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Process signals and best-effort cancellation; connect installs no handlers.

use crate::{error::Error, output::Output};
use darkbio_connect::{Client, Closer, schema};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared cancellation registration used by commands and platform signal handlers.
#[derive(Clone)]
pub(crate) struct Interrupt(Arc<Mutex<State>>);

/// Active connection and cancellation target, held stable throughout interruption.
#[derive(Default)]
struct State {
    /// Request and shutdown handles for the currently selected Ark session.
    connection: Option<(Client, Closer)>,
    /// Task or dataset upload whose cancellation can be attempted explicitly.
    target: Option<Target>,
    /// Latest structured result to preserve if interruption precedes completion.
    partial: Option<Value>,
}

/// Device-side work addressable by an explicit cancellation request.
#[derive(Clone, Copy)]
pub(crate) enum Target {
    /// App task ID covering both its upload and execution.
    Task(u64),
    /// Dataset upload session ID, including processing.
    Upload(u64),
}

impl Interrupt {
    /// Registers platform handlers and an initially empty cancellation state.
    ///
    /// Cancellation runs on a worker or OS callback thread, outside a Unix
    /// signal handler.
    pub fn install(output: Output) -> Result<Self, Error> {
        let interrupt = Self(Arc::new(Mutex::new(State::default())));

        // Watch SIGINT and SIGTERM on a thread of its own, exiting with the
        // shell's codes 130 and 143
        #[cfg(unix)]
        {
            let handle = interrupt.clone();
            use signal_hook::{
                consts::{SIGINT, SIGTERM},
                iterator::Signals,
            };
            let mut signals = Signals::new([SIGINT, SIGTERM])?;
            std::thread::Builder::new()
                .name("ark-signals".into())
                .spawn(move || {
                    if let Some(signal) = signals.forever().next() {
                        handle.cancel(output, if signal == SIGTERM { 143 } else { 130 });
                    }
                })?;
        }

        // Windows console callbacks carry no context, so the state lives in a
        // static the callback reads
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            if CONSOLE.set((interrupt.clone(), output)).is_err() {
                return Err(Error::new(1, "io", "console handler already installed"));
            }
            // Windows invokes the callback on its own thread. Keep it running
            // through cancellation; returning from a close event ends the process.
            // Registering is sound, since the handler is a plain function that
            // lives as long as the process.
            if unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(interrupt)
    }

    /// Registers a new session and clears any cancellation target from the
    /// previous one.
    pub fn connection(&self, client: Client, closer: Closer) {
        let mut state = self.0.lock().expect("cancellation not poisoned");
        state.connection = Some((client, closer));
        state.target = None;
    }

    /// Records device-side work as soon as the Ark returns its cancellation ID.
    pub fn target(&self, target: Target) {
        self.0.lock().expect("cancellation not poisoned").target = Some(target);
    }

    /// Replaces the result snapshot used if a signal interrupts the command.
    pub fn partial(&self, value: Value) {
        self.0.lock().expect("cancellation not poisoned").partial = Some(value);
    }

    /// Clears completed work while retaining the session and partial result.
    pub fn clear(&self) {
        self.0.lock().expect("cancellation not poisoned").target = None;
    }

    /// Synchronizes command completion with a handler already holding
    /// cancellation state.
    ///
    /// A cancellation in progress ends the process, so this then never returns.
    pub fn finished(&self) {
        drop(self.0.lock().expect("cancellation not poisoned"));
    }

    /// Attempts bounded cancellation, closes the session and exits with the
    /// signal code.
    ///
    /// The state lock prevents commands from replacing the target during
    /// cleanup. The work gets 5 s to confirm its cancellation, and a JSON
    /// invocation ends with the last partial result when there is one.
    fn cancel(&self, output: Output, code: i32) -> ! {
        // Hold the state until the process exits, so commands cannot swap the
        // target
        let state = self.0.lock().expect("cancellation not poisoned");
        output.event("note", "interrupted; cancelling active work");

        // Ask the Ark to cancel the registered work, then close the session
        if let Some((client, closer)) = &state.connection {
            let timeout = Duration::from_secs(5);
            let result = match state.target {
                Some(Target::Task(taskid)) => client
                    .call_timeout(schema::ExecutionCancelRequest { taskid }, timeout)
                    .map(drop),
                Some(Target::Upload(session)) => client
                    .call_timeout(schema::SlotUploadCancelRequest { session }, timeout)
                    .map(drop),
                None => Ok(()),
            };
            if let Err(error) = result {
                output.event(
                    "warning",
                    format!("could not confirm cancellation: {error}"),
                );
            }
            closer.close();
        }

        // Report the interruption, keeping a partial JSON result when one exists
        let error = Error::new(
            code as u8,
            if code == 143 {
                "terminated"
            } else {
                "interrupted"
            },
            "command interrupted",
        );
        if output.json() {
            let fallback = json!({"error":error.json()});
            let _ = output.document(state.partial.as_ref().unwrap_or(&fallback));
        }
        output.error(&error);
        output.finish();
        std::process::exit(code)
    }
}

/// Callback state kept for the process lifetime, since Windows callbacks have
/// no context pointer.
#[cfg(windows)]
static CONSOLE: std::sync::OnceLock<(Interrupt, Output)> = std::sync::OnceLock::new();

/// Handles console interruption on the OS callback thread and leaves unknown
/// events unclaimed.
///
/// # Safety
///
/// Windows calls it as a console control handler, with the control event as
/// its only argument, after [`Interrupt::install`] registers it. It has no
/// other requirements.
#[cfg(windows)]
unsafe extern "system" fn console_handler(event: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    let code = match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => 130,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => 143,
        _ => return 0,
    };
    if let Some((interrupt, output)) = CONSOLE.get() {
        interrupt.cancel(output.clone(), code);
    }
    0
}

/// Tests of the Unix signal handling.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Checks that a signal before any task still ends with a complete JSON
    /// failure and the shell's conventional exit class.
    ///
    /// The parent waits for a step event before signaling, so the test covers
    /// the handler rather than process startup.
    #[cfg(unix)]
    #[test]
    fn signals_finish_the_json_document() {
        use clap::Parser;

        // As the child, emit one event of each kind, then wait for the signal
        if std::env::var_os("ARK_TEST_SIGNAL_CHILD").is_some() {
            let options = crate::args::Cli::parse_from(["ark", "--json", "-v"]).options;
            let output = Output::new(&options);
            let _interrupt = Interrupt::install(output.clone()).unwrap();
            for kind in ["progress", "note", "warning", "approve", "hint", "step"] {
                output.event(kind, "first line\nsecond line");
            }
            output.event_value(json!({"event":"log", "level":"debug", "target":"darkbio_connect", "fields":{"message":"first line\nsecond line"}}));
            output.event("step", "ready for signal");
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
            panic!("child input closed before signal");
        }

        // As the parent, run the child once per signal
        use std::{
            io::{BufRead, BufReader, Read},
            process::{Command, Stdio},
        };
        for (signal, expected) in [("-INT", 130), ("-TERM", 143)] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "interrupt::tests::signals_finish_the_json_document",
                    "--nocapture",
                ])
                .env("ARK_TEST_SIGNAL_CHILD", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();

            // Every event arrives as one JSON line, the last one marking the
            // handler as installed
            let mut stderr = BufReader::new(child.stderr.take().unwrap());
            let mut event = String::new();
            for kind in [
                "progress", "note", "warning", "approve", "hint", "step", "log", "step",
            ] {
                event.clear();
                assert!(stderr.read_line(&mut event).unwrap() > 0);
                let event: Value = serde_json::from_str(&event).unwrap();
                assert_eq!(event["event"], kind);
            }
            assert_eq!(
                serde_json::from_str::<Value>(&event).unwrap()["message"],
                "ready for signal"
            );

            // The signal ends the child with one pretty JSON error, the
            // signal's code and a final error event
            assert!(
                Command::new("kill")
                    .args([signal, &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let mut stdout = Vec::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_end(&mut stdout)
                .unwrap();
            let start = stdout.iter().position(|byte| *byte == b'{').unwrap();
            let document: Value = serde_json::from_slice(&stdout[start..]).unwrap();
            assert_eq!(
                &stdout[start..],
                format!("{}\n", serde_json::to_string_pretty(&document).unwrap()).as_bytes()
            );
            assert_eq!(
                document["error"]["code"],
                if expected == 143 {
                    "terminated"
                } else {
                    "interrupted"
                }
            );
            assert_eq!(child.wait().unwrap().code(), Some(expected));
            let events: Vec<Value> = stderr
                .lines()
                .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                .collect();
            assert_eq!(events.last().unwrap()["event"], "error");
        }
    }
}
