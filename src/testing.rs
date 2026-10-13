// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Helpers shared by the command modules' tests, for test clocks and for
//! capturing narration.

use crate::output::Output;
use darkbio_clock::TestClock;
use std::io::Write;
use std::thread;
use std::time::Instant;

/// What a scenario printed in one mode, on a terminal, in a pipe or as JSON.
pub(crate) struct Narration {
    /// Whether the scenario ran with `--json`.
    pub json: bool,
    /// Whether stderr was a terminal that redraws in place.
    pub terminal: bool,
    /// Stdout, without the test harness's own lines.
    pub stdout: String,
    /// Stderr, terminal redraws included.
    pub stderr: String,
}

/// Runs `run` on a test clock once per mode, terminal, pipe and JSON, each in
/// a child process, and returns what each printed.
///
/// `test` is the calling test's path, which each child runs again. The child
/// prints through the real output, so the captures hold the streams a person
/// or a parser reads.
pub(crate) fn narration(test: &str, run: impl FnOnce(Output, &mut TestClock)) -> Vec<Narration> {
    // As the child, run the scenario once in the mode the parent asked for
    if let Ok(mode) = std::env::var("ARK_TEST_COMMAND_NARRATION") {
        let mut clock = TestClock::new();
        let output = Output::test(mode == "json", mode == "terminal", clock.clock());
        writeln!(std::io::stdout(), "<result>").unwrap();
        run(output, &mut clock);
        writeln!(std::io::stdout(), "</result>").unwrap();
        return Vec::new();
    }

    // As the parent, run a child per mode and collect what each printed
    ["terminal", "pipe", "json"]
        .into_iter()
        .map(|mode| {
            let captured = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("ARK_TEST_COMMAND_NARRATION", mode)
                .output()
                .unwrap();
            assert!(captured.status.success(), "{test}: {captured:?}");
            let stdout = String::from_utf8(captured.stdout).unwrap();
            Narration {
                json: mode == "json",
                terminal: mode == "terminal",
                stdout: stdout
                    .split_once("<result>\n")
                    .unwrap()
                    .1
                    .split_once("</result>\n")
                    .unwrap()
                    .0
                    .into(),
                stderr: String::from_utf8(captured.stderr).unwrap(),
            }
        })
        .collect()
}

impl Narration {
    /// Parses stderr as JSON events, failing on any line that is not one.
    pub fn events(&self) -> Vec<serde_json::Value> {
        assert!(self.json);
        self.stderr
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Asserts that the messages appear in this order, in the events or on
    /// stderr.
    pub fn contains(&self, expected: &[&str]) {
        let text = if self.json {
            self.events()
                .iter()
                .map(|event| event["message"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            self.stderr.clone()
        };
        let mut remaining = text.as_str();
        for expected in expected {
            remaining = remaining
                .split_once(expected)
                .unwrap_or_else(|| panic!("missing {expected:?} in {text:?}"))
                .1;
        }
    }

    /// Asserts the action and location of every approve event in order, under
    /// JSON.
    pub fn approvals(&self, expected: &[(&str, Option<&str>)]) {
        if self.json {
            let events = self.events();
            let actual: Vec<_> = events
                .iter()
                .filter(|event| event["event"] == "approve")
                .map(|event| (event["action"].as_str().unwrap(), event["where"].as_str()))
                .collect();
            assert_eq!(actual, expected);
            assert!(
                events
                    .iter()
                    .filter(|event| event["event"] == "approve")
                    .all(|event| event.get("where").is_some())
            );
        }
    }
}

/// Blocks until the earliest wait or timer on the clock is due at `deadline`.
///
/// The advance that reaches the deadline then wakes it, whenever the test makes
/// that advance.
pub(crate) fn wait_deadline(tester: &TestClock, deadline: Instant) {
    while tester.next_deadline() != Some(deadline) {
        thread::yield_now();
    }
}
