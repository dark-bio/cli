// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! App commands, which run an app on the Ark and report its result, or cancel
//! a task.

use crate::{
    args,
    context::{Context, open_file},
    error::Error,
    interrupt::Target,
    progress::Transfer,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use darkbio_connect::{ExecutionProgress, schema};
use serde_json::{Value, json};

/// Cancels an explicit task or uploads and runs a local app after unlock.
///
/// The connection library owns protocol sequencing, and the CLI owns progress,
/// partial results and byte-preserving report output. An app failure retains
/// its returned result.
pub(crate) fn run(context: &Context, command: args::App) -> Result<(), Error> {
    // A cancel request goes straight to the Ark and reports the task it named
    let args::App::Run { file: path } = command else {
        let args::App::Cancel { task } = command else {
            unreachable!()
        };
        let connection = context.connect(None)?;
        connection.client.call(
            schema::ExecutionCancelRequest { taskid: task },
            context.timing(),
        )?;
        return context
            .output
            .document(&json!({"task":task.to_string(),"cancelled":true}));
    };

    // Open the app before connecting, then require an unlocked Ark
    let (mut file, size) = open_file(&path)?;
    let connection = context.connect(None)?;
    context.require_unlocked(&connection, false)?;

    // The result starts unknown and fills in as the run goes. Running progress
    // repeats at most every second on a terminal, and every 5 s elsewhere.
    let mut value = json!({"task":null,"app":{"name":null,"version":null},"success":null,"stdout":null,"stderr":null,"duration_seconds":null});
    let clock = connection.client.clock();
    let mut started = None;
    let mut transfer = Transfer::new(context.output.terminal(), clock.clone());
    let report_interval = if context.output.terminal() { 1 } else { 5 };
    let mut reported = None;

    // Upload and run the app, registering the task for interruption as soon as
    // the Ark names it
    let result =
        connection
            .client
            .execute(size, &mut file, context.timing(), |stage| match stage {
                ExecutionProgress::Preparing => {
                    context.output.event("progress", "preparing app upload")
                }
                ExecutionProgress::Started { taskid } => {
                    value["task"] = json!(taskid.to_string());
                    context.interrupt.target(Target::Task(taskid));
                    context.interrupt.partial(value.clone());
                    context.output.event("note", format!("task {taskid}"));
                }
                ExecutionProgress::Uploading { uploaded, total } => {
                    if let Some(line) = transfer.update(uploaded, total) {
                        context.output.progress(&line);
                    }
                }
                ExecutionProgress::Authorizing => context.output.event(
                    "approve",
                    format!("run {} (Ark Companion on your phone)", path.display()),
                ),
                ExecutionProgress::Running { elapsed } => {
                    started.get_or_insert_with(|| clock.now() - elapsed);
                    let seconds = elapsed.as_secs();
                    if reported.is_none_or(|last| seconds >= last + report_interval) {
                        context
                            .output
                            .event("progress", format!("running: {seconds} s elapsed"));
                        reported = Some(seconds);
                    }
                }
            });

    // A failed run still prints the partial JSON result once a task started
    context.interrupt.clear();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if context.output.json() && !value["task"].is_null() {
                context.output.document(&value)?;
            }
            return Err(error.into());
        }
    };

    // Complete the result, keeping output that is not UTF-8 as base64
    value["app"] = json!({"name":result.app_name,"version":result.app_version});
    value["success"] = json!(result.success);
    let duration = clock.elapsed(started.expect("successful execution reported running"));
    value["duration_seconds"] = json!(duration.as_secs());
    bytes(&mut value, "stdout", &result.stdout);
    bytes(&mut value, "stderr", &result.stderr);

    // Print the report as it came, then fail when the app reported failure
    if context.output.json() {
        context.output.document(&value)?;
    } else {
        context.output.app(&result.stdout, &result.stderr)?;
    }
    context.output.event(
        "note",
        format!(
            "{} {} finished in {} s",
            result.app_name,
            result.app_version,
            duration.as_secs()
        ),
    );
    if result.success {
        Ok(())
    } else {
        Err(Error::new(8, "app-failed", "the app reported failure"))
    }
}

/// Stores valid UTF-8 verbatim; other bytes replace the text key with a base64
/// sibling.
fn bytes(value: &mut Value, name: &str, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => value[name] = json!(text),
        Err(_) => {
            let fields = value.as_object_mut().expect("result object");
            let index = fields
                .keys()
                .position(|key| key == name)
                .expect("stream field");
            fields.shift_remove(name);
            fields.shift_insert(
                index,
                format!("{name}_base64"),
                json!(BASE64_STANDARD.encode(bytes)),
            );
        }
    }
}

/// Tests of the app result encoding.
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that output that is not UTF-8 becomes base64 in the same key
    /// position, while UTF-8 output stays text.
    #[test]
    fn app_output_is_never_lossily_decoded() {
        let mut result = json!({"task":u64::MAX.to_string(),"stdout":null,"stderr":null});
        bytes(&mut result, "stdout", &[0, 255, 128]);
        bytes(&mut result, "stderr", b"hello\n\0");
        assert_eq!(result["task"], "18446744073709551615");
        assert!(result.get("stdout").is_none());
        assert_eq!(result["stdout_base64"], "AP+A");
        assert_eq!(result["stderr"], "hello\n\0");
        assert_eq!(
            result
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["task", "stdout_base64", "stderr"]
        );
    }
}
