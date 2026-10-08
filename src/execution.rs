// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The app command, which runs an app on the Ark and reports its result.

use crate::{
    args,
    context::{Context, open_file},
    error::Error,
    interrupt::Target,
    output::Output,
    progress::Transfer,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use darkbio_connect::{ExecutionOutcome, ExecutionProgress};
use serde_json::{Value, json};

/// Uploads and runs a local app after unlock.
///
/// The connection library owns protocol sequencing, and the CLI owns progress,
/// partial results and byte-preserving report output. An app failure retains
/// its returned result.
pub(crate) fn run(context: &Context, command: args::App) -> Result<(), Error> {
    // Open the app before connecting, then require an unlocked Ark
    let args::App::Run { file: path } = command;
    let (mut file, size) = open_file(&path)?;
    let connection = context.connect(None)?;
    context.require_unlocked(&connection, false)?;

    // The result starts unknown and fills in as the run goes. Running progress
    // repeats at most every second on a terminal, and every 5 s elsewhere.
    let mut value = partial();
    let clock = connection.client.clock();
    let mut transfer = Transfer::new(context.output.terminal(), clock);
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
                    let seconds = elapsed.as_secs();
                    if reported.is_none_or(|last| seconds >= last + report_interval) {
                        context
                            .output
                            .event("progress", format!("running: {seconds} s elapsed"));
                        reported = Some(seconds);
                    }
                }
                ExecutionProgress::Reviewing => context.output.event(
                    "approve",
                    format!(
                        "review the report of {} (Ark Companion on your phone)",
                        path.display()
                    ),
                ),
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
    print(&context.output, value, result)
}

/// Builds the result document before the task or its released report is known.
fn partial() -> Value {
    json!({"task":null,"app":{"name":null,"version":null,"develop":null},"success":null,"paths":null,"media":null,"stdout":null,"stderr":null,"duration_seconds":null})
}

/// Prints a released report and its duration, retaining an unsuccessful result.
fn print(output: &Output, mut value: Value, outcome: ExecutionOutcome) -> Result<(), Error> {
    // Complete the result, keeping output that is not UTF-8 as base64
    let result = &outcome.result;
    value["app"] = json!({"name":result.name,"version":result.version,"develop":result.develop});
    value["success"] = json!(result.success);
    value["paths"] = json!(result.paths);
    value["media"] = json!(result.media);
    value["duration_seconds"] = json!(outcome.duration.as_secs());
    bytes(&mut value, "stdout", &outcome.stdout);
    bytes(&mut value, "stderr", &outcome.stderr);

    // Print the report as it came, then fail when the app reported failure
    if output.json() {
        output.document(&value)?;
    } else {
        output.app(&outcome.stdout, &outcome.stderr)?;
    }
    output.event(
        "note",
        format!(
            "{} {} finished in {} s",
            result.name,
            result.version,
            outcome.duration.as_secs()
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
    use clap::Parser;
    use darkbio_connect::schema;
    use std::io::Write;
    use std::time::Duration;

    /// Released and partial reports preserve the JSON contract and failure class.
    #[test]
    fn test_report_output() {
        // Child scenarios print through the command's output layer
        if let Ok(scenario) = std::env::var("ARK_TEST_REPORT_SCENARIO") {
            let options = args::Cli::parse_from(["ark", "--json"]).options;
            let output = Output::new(&options);
            let mut value = partial();
            value["task"] = json!("7");
            writeln!(std::io::stdout(), "<result>").unwrap();
            if scenario == "partial" {
                output.document(&value).unwrap();
            } else {
                let outcome = ExecutionOutcome {
                    result: schema::ExecutionResultResponse {
                        name: "sample app".into(),
                        version: "1.2.3".into(),
                        develop: true,
                        success: scenario == "success",
                        paths: vec!["v1/sample".into()],
                        media: "text/plain".into(),
                        stdout_bytes: 3,
                        stderr_bytes: 6,
                    },
                    stdout: vec![0, 255, 128],
                    stderr: b"hello\n".to_vec(),
                    duration: Duration::from_secs(7),
                };
                let result = print(&output, value, outcome);
                if scenario == "failure" {
                    let error = result.unwrap_err();
                    assert_eq!((error.class, error.code), (8, "app-failed"));
                    output.error(&error);
                } else {
                    result.unwrap();
                }
            }
            writeln!(std::io::stdout(), "</result>").unwrap();
            return;
        }

        // Capture complete documents and events without a device or real clock
        for scenario in ["partial", "success", "failure"] {
            let captured = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "execution::tests::test_report_output",
                    "--nocapture",
                ])
                .env("ARK_TEST_REPORT_SCENARIO", scenario)
                .output()
                .unwrap();
            assert!(captured.status.success(), "{captured:?}");
            let stdout = String::from_utf8(captured.stdout).unwrap();
            let document = stdout
                .split_once("<result>\n")
                .unwrap()
                .1
                .split_once("</result>\n")
                .unwrap()
                .0;
            let actual: Value = serde_json::from_str(document).unwrap();
            if scenario == "partial" {
                assert_eq!(
                    actual,
                    json!({
                        "task":"7", "app":{"name":null,"version":null,"develop":null},
                        "success":null, "paths":null, "media":null, "stdout":null,
                        "stderr":null, "duration_seconds":null,
                    })
                );
                assert!(captured.stderr.is_empty());
                continue;
            }
            assert_eq!(
                actual,
                json!({
                    "task":"7", "app":{"name":"sample app","version":"1.2.3","develop":true},
                    "success":scenario == "success", "paths":["v1/sample"], "media":"text/plain",
                    "stdout_base64":"AP+A", "stderr":"hello\n", "duration_seconds":7,
                })
            );
            let stderr = String::from_utf8(captured.stderr).unwrap();
            let events: Vec<Value> = stderr
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                events[0],
                json!({"event":"note","message":"sample app 1.2.3 finished in 7 s"})
            );
            assert_eq!(events.len(), if scenario == "failure" { 2 } else { 1 });
            if scenario == "failure" {
                assert_eq!(events[1]["event"], "error");
                assert_eq!(events[1]["error"]["code"], "app-failed");
            }
        }
    }

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
