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

#[derive(Clone)]
pub(crate) struct Interrupt(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    connection: Option<(Client, Closer)>,
    target: Option<Target>,
    partial: Option<Value>,
}
#[derive(Clone, Copy)]
pub(crate) enum Target {
    Task(u64),
    Upload(u64),
}

impl Interrupt {
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
    pub fn connection(&self, client: Client, closer: Closer) {
        let mut state = self.0.lock().expect("cancellation not poisoned");
        state.connection = Some((client, closer));
        state.target = None;
    }
    pub fn target(&self, target: Target) {
        self.0.lock().expect("cancellation not poisoned").target = Some(target);
    }
    pub fn partial(&self, value: Value) {
        self.0.lock().expect("cancellation not poisoned").partial = Some(value);
    }
    pub fn clear(&self) {
        self.0.lock().expect("cancellation not poisoned").target = None;
    }
    pub fn finished(&self) {
        drop(self.0.lock().expect("cancellation not poisoned"));
    }
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

#[cfg(windows)]
static CONSOLE: std::sync::OnceLock<(Interrupt, Output)> = std::sync::OnceLock::new();

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
