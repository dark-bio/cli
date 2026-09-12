// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Process-level contracts that require no device or network access.

use serde_json::Value;
use std::process::{Command, Output};

fn ark(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ark"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn usage_errors_are_json_in_both_streams() {
    for args in [
        vec![
            "data",
            "fetch",
            "--all",
            "reference-genome",
            "--format",
            "json",
        ],
        vec![
            "--format=json",
            "data",
            "upload",
            "x",
            "--dry-run",
            "--unlock",
        ],
        vec!["--format", "json", "--timeout", "0", "status"],
        vec!["--format", "json", "app", "cancel", "18446744073709551616"],
        vec!["--format", "json", "--version", "status"],
        vec!["--format", "json", "help", "missing"],
    ] {
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["error"]["code"], "usage");
        let events: Vec<Value> = String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["event"], "error");
        let message = document["error"]["message"].as_str().unwrap();
        assert!(!message.contains("Usage:"), "{message}");
    }
}

#[test]
fn version_is_a_single_structured_result() {
    let output = ark(&["--version", "--format", "json"]);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["tool"], env!("CARGO_PKG_VERSION"));
    assert_eq!(document["connect"], darkbio_connect::VERSION);
    assert_eq!(document["wire"], darkbio_connect::wire::VERSION);
    assert!(document.get("environments").is_none());
    semver::Version::parse(document["minimum_firmware"].as_str().unwrap()).unwrap();
    chrono::DateTime::parse_from_rfc3339(document["minimum_develop_publish"].as_str().unwrap())
        .unwrap();
    assert!(output.stderr.is_empty());
}

#[test]
fn help_matches_the_supported_palette() {
    let output = ark(&["--help"]);
    assert!(output.status.success());
    let root = String::from_utf8(output.stdout).unwrap();
    assert!(root.lines().count() <= 40, "{root}");
    for path in [
        "status",
        "data paths",
        "data upload",
        "data fetch",
        "firmware update",
        "app run",
    ] {
        let mut args: Vec<_> = path.split(' ').collect();
        args.push("--help");
        let output = ark(&args);
        assert!(output.status.success(), "{output:?}");
        let long = String::from_utf8(output.stdout).unwrap();
        for field in [
            "Requires:",
            "Approval:",
            "Time:",
            "Prints:",
            "Exit:",
            "Examples:",
        ] {
            assert!(long.contains(field), "{path}: {long}");
        }
        assert!(!long.contains("--timeout <SECONDS>"));
        let mut command = vec!["help"];
        command.extend(path.split(' '));
        assert_eq!(ark(&command).stdout, long.as_bytes());
        *args.last_mut().unwrap() = "-h";
        assert!(
            !String::from_utf8(ark(&args).stdout)
                .unwrap()
                .contains("Requires:")
        );
    }
    let manual = String::from_utf8(ark(&["help", "--all"]).stdout).unwrap();
    for absent in ["ark app check", "ark lock"] {
        assert!(!manual.contains(absent), "{absent} advertised prematurely");
    }
    assert!(
        !String::from_utf8(ark(&["status", "-h"]).stdout)
            .unwrap()
            .contains("--pubkey")
    );
    assert!(
        String::from_utf8(ark(&["status", "--help"]).stdout)
            .unwrap()
            .contains("--pubkey")
    );
}

#[test]
fn completions_are_generated_for_the_binary_name() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = ark(&["completions", shell]);
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("ark"));
        assert!(output.stderr.is_empty());
    }
}

/// A signal before a task exists still produces a complete JSON failure and
/// the shell's conventional exit class. Wait for a step event before signalling
/// so this exercises our handler rather than process startup.
#[cfg(unix)]
#[test]
fn signals_finish_the_json_document() {
    use std::{
        io::{BufRead, BufReader, Read},
        process::Stdio,
    };
    for (signal, expected) in [("-INT", 130), ("-TERM", 143)] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ark"))
            .args(["enroll", "--cwt", "/dev/stdin", "--format", "json", "-v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut event = String::new();
        stderr.read_line(&mut event).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&event).unwrap()["message"],
            "reading attestation"
        );
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
        let document: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            document["error"]["code"],
            if expected == 143 {
                "terminated"
            } else {
                "interrupted"
            }
        );
        assert_eq!(child.wait().unwrap().code(), Some(expected));
    }
}
