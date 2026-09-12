// ark: command line for Dark Bio Arks
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! App results and CLI cancellation handles.

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
use std::time::Instant;

pub(crate) fn run(context: &Context, command: args::App) -> Result<(), Error> {
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
    let (mut file, size) = open_file(&path)?;
    let connection = context.connect(None)?;
    context.require_unlocked(&connection, false)?;
    let mut value = json!({"task":null,"app":{"name":null,"version":null},"success":null,"stdout":null,"stderr":null,"duration_seconds":null});
    let mut started = None;
    let mut transfer = Transfer::new(context.output.human());
    let report_interval = if context.output.human() { 1 } else { 5 };
    let mut reported = None;
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
                        context.output.event("progress", line);
                    }
                }
                ExecutionProgress::Authorizing => context.output.event(
                    "approve",
                    format!("run {} (Ark Companion on your phone)", path.display()),
                ),
                ExecutionProgress::Running { elapsed } => {
                    started.get_or_insert_with(|| Instant::now() - elapsed);
                    let seconds = elapsed.as_secs();
                    if reported.is_none_or(|last| seconds >= last + report_interval) {
                        context
                            .output
                            .event("progress", format!("running: {seconds} s elapsed"));
                        reported = Some(seconds);
                    }
                }
            });
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
    value["app"] = json!({"name":result.app_name,"version":result.app_version});
    value["success"] = json!(result.success);
    let duration = started
        .expect("successful execution reported running")
        .elapsed();
    value["duration_seconds"] = json!(duration.as_secs());
    bytes(&mut value, "stdout", &result.stdout);
    bytes(&mut value, "stderr", &result.stderr);
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

fn bytes(value: &mut Value, name: &str, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => value[name] = json!(text),
        Err(_) => {
            value.as_object_mut().expect("result object").remove(name);
            value[format!("{name}_base64")] = json!(BASE64_STANDARD.encode(bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_output_is_never_lossily_decoded() {
        let mut result = json!({"task":u64::MAX.to_string(),"stdout":null,"stderr":null});
        bytes(&mut result, "stdout", &[0, 255, 128]);
        bytes(&mut result, "stderr", b"hello\n\0");
        assert_eq!(result["task"], "18446744073709551615");
        assert!(result.get("stdout").is_none());
        assert_eq!(result["stdout_base64"], "AP+A");
        assert_eq!(result["stderr"], "hello\n\0");
    }
}
