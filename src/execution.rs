// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The run command, which runs an app on the Ark and reports its result.

use crate::{
    context::{Context, open_file},
    error::Error,
    interrupt::Target,
    output::Output,
    progress::Update,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use darkbio_connect::{ExecutionOutcome, ExecutionProgress};
use serde_json::{Value, json};
use std::path::Path;

/// Loads and runs the app at `path` after unlock.
///
/// The connection library owns protocol sequencing, and the CLI owns progress,
/// partial results and byte-preserving report output. An app failure retains
/// its returned result.
pub(crate) fn run(context: &Context, path: &Path) -> Result<(), Error> {
    // Open the app before connecting, then require an unlocked Ark
    let (mut file, _) = open_file(path)?;
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let mut connection = context.connect(None)?;
    context.require_unlocked(&connection, false)?;

    // The result starts unknown and fills in as the task goes
    let mut value = partial();
    let mut narration = Narration::new(&context.output, &name);

    // Load and run the app, registering the task for interruption as soon as
    // the Ark names it
    let result = connection
        .ark
        .execute(&mut file, context.timing(), |stage| {
            if let ExecutionProgress::Started { taskid } = stage {
                value["task"] = json!(taskid.to_string());
                context.interrupt.target(Target::Task(taskid));
                context.interrupt.partial(value.clone());
            }
            narration.update(stage);
        });

    // A failed command still prints the partial JSON result once a task started
    context.interrupt.clear();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let error = Error::from(error);
            context.output.fail_stage(Some(&error));
            if context.output.json() && !value["task"].is_null() {
                context.output.document(&value)?;
            }
            return Err(error);
        }
    };
    narration.finish(result.duration);
    print(&context.output, value, result)
}

/// Narration of an app run, from loading the file to the owner's review.
struct Narration<'a> {
    /// Output the narration goes to.
    output: &'a Output,
    /// File name of the app, as the instructions and outcomes name it.
    name: &'a str,
    /// Whether the review began, which ended the running stage.
    reviewed: bool,
}

impl<'a> Narration<'a> {
    /// Prepares to narrate loading and running the named app.
    fn new(output: &'a Output, name: &'a str) -> Self {
        Self {
            output,
            name,
            reviewed: false,
        }
    }

    /// Narrates one step of the run, keeping the run time the connection
    /// reports.
    fn update(&mut self, stage: ExecutionProgress) {
        let name = self.name;
        match stage {
            ExecutionProgress::Preparing => {
                self.output
                    .stage("preparing", &format!("Preparing {name}"), None)
            }
            ExecutionProgress::Started { taskid } => {
                self.output.event("step", format!("task {taskid}"));
            }
            ExecutionProgress::Loading => self.output.stage(
                "loading",
                &format!("Loading {name}"),
                Some(&format!("Loaded {name}")),
            ),
            ExecutionProgress::Authorizing => self.output.approve(
                "run",
                Some("phone"),
                format!("Approve running {name} on your phone, in Ark Companion"),
            ),
            ExecutionProgress::Running { elapsed } => self.running(elapsed),
            ExecutionProgress::Reviewing { elapsed } => {
                self.running(elapsed);
                self.reviewed = true;
                self.output.approve(
                    "review",
                    Some("phone"),
                    "Review the report on your phone, in Ark Companion",
                );
            }
        }
    }

    /// Starts or updates the running stage with its timer at `elapsed`, also for
    /// a run that ended before its first poll.
    fn running(&self, elapsed: std::time::Duration) {
        self.output.progress(&Update {
            completed: Some(format!("Ran {}", self.name)),
            elapsed: Some(elapsed),
            ..Update::new("running", &format!("Running {}", self.name))
        });
    }

    /// Ends the review, or the run when its report arrived without a review
    /// poll.
    fn finish(&self, elapsed: std::time::Duration) {
        if !self.reviewed {
            self.running(elapsed);
        }
        self.output.end_stage(None, None);
    }
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

    // Name the app as a `-v` step, then print its report, failing after it when
    // the app failed
    output.event("step", format!("{} {}", result.name, result.version));
    if output.json() {
        output.document(&value)?;
    } else {
        output.app(&outcome.stdout, &outcome.stderr)?;
    }
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
    use crate::args;
    use clap::Parser;
    use darkbio_connect::schema;
    use std::io::Write;
    use std::time::Duration;

    /// An app run names its file, its approvals' places and its run time.
    #[test]
    fn test_app_narration() {
        for captured in
            crate::testing::narration("execution::tests::test_app_narration", |output, clock| {
                let mut narration = Narration::new(&output, "cilantro.wasm");
                narration.update(ExecutionProgress::Preparing);
                narration.update(ExecutionProgress::Started { taskid: 7 });
                narration.update(ExecutionProgress::Loading);
                output.finish();
                clock.advance(Duration::from_secs(2));
                narration.update(ExecutionProgress::Authorizing);
                output.finish();
                clock.advance(Duration::from_secs(46));
                narration.update(ExecutionProgress::Running {
                    elapsed: Duration::from_secs(5),
                });
                output.finish();
                clock.advance(Duration::from_secs(1));
                narration.update(ExecutionProgress::Running {
                    elapsed: Duration::from_secs(6),
                });
                output.finish();
                clock.advance(Duration::from_secs(6));
                narration.update(ExecutionProgress::Reviewing {
                    elapsed: Duration::from_secs(12),
                });
                output.finish();
                clock.advance(Duration::from_secs(20));
                narration.finish(Duration::from_secs(12));
            })
        {
            captured.contains(&[
                "Preparing cilantro.wasm",
                "Loading cilantro.wasm",
                "Loaded cilantro.wasm · 2s",
                "Approve running cilantro.wasm on your phone, in Ark Companion",
                "Approved on your phone · 41s",
                "Running cilantro.wasm · 5s",
                "Ran cilantro.wasm · 12s",
                "Review the report on your phone, in Ark Companion",
                "Approved on your phone · 20s",
            ]);
            captured.approvals(&[("run", Some("phone")), ("review", Some("phone"))]);
            assert!(!captured.stderr.contains("task 7"));
            assert!(!captured.stderr.contains("✓ Preparing"));
            assert!(!captured.stderr.contains("Uploaded"));
            assert!(!captured.stderr.contains("finished in"));
            if !captured.terminal {
                assert!(!captured.stderr.contains("Running cilantro.wasm · 6s"));
            }
            assert!(captured.stdout.is_empty());
        }
    }

    /// A run too fast for a running or review poll still prints its outcome.
    #[test]
    fn test_fast_app_narration() {
        for captured in crate::testing::narration(
            "execution::tests::test_fast_app_narration",
            |output, _clock| {
                for review in [true, false] {
                    let mut narration = Narration::new(&output, "fast.wasm");
                    narration.update(ExecutionProgress::Authorizing);
                    if review {
                        narration.update(ExecutionProgress::Reviewing {
                            elapsed: Duration::from_secs(3),
                        });
                    }
                    narration.finish(Duration::from_secs(3));
                }
            },
        ) {
            captured.contains(&["Ran fast.wasm · 3s", "Ran fast.wasm · 3s"]);
            captured.approvals(&[
                ("run", Some("phone")),
                ("review", Some("phone")),
                ("run", Some("phone")),
            ]);
        }
    }

    /// Released and partial reports preserve the JSON contract and failure class.
    #[test]
    fn test_report_output() {
        // Child scenarios print through the command's output layer
        if let Ok(scenario) = std::env::var("ARK_TEST_REPORT_SCENARIO") {
            let options = args::Cli::parse_from(["ark", "--json", "-v"]).options;
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
                json!({"event":"step","message":"sample app 1.2.3"})
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
