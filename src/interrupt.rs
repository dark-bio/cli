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
    /// Cancellation runs on a worker or OS callback thread, outside a Unix signal handler.
    pub fn install(output: Output) -> Result<Self, Error> {
        let interrupt = Self(Arc::new(Mutex::new(State::default())));
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
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            if CONSOLE.set((interrupt.clone(), output)).is_err() {
                return Err(Error::new(1, "io", "console handler already installed"));
            }
            // Windows invokes the callback on its own thread. Keep it running
            // through cancellation; returning from a close event ends the process.
            if unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(interrupt)
    }
    /// Registers a new session and clears any cancellation target from the previous one.
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
    /// Synchronizes command completion with a handler already holding cancellation state.
    pub fn finished(&self) {
        drop(self.0.lock().expect("cancellation not poisoned"));
    }
    /// Attempts bounded cancellation, closes the session and exits with the signal code.
    /// The state lock prevents commands from replacing the target during cleanup.
    fn cancel(&self, output: Output, code: i32) -> ! {
        let state = self.0.lock().expect("cancellation not poisoned");
        output.event("note", "interrupted; cancelling active work");
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

/// Retains callback state for the process lifetime; Windows callbacks have no context pointer.
#[cfg(windows)]
static CONSOLE: std::sync::OnceLock<(Interrupt, Output)> = std::sync::OnceLock::new();

/// Handles console interruption on the OS callback thread and leaves unknown events unclaimed.
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
