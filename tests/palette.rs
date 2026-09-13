// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Process-level contracts that require no device or network access.

use serde_json::Value;
use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::sync::Mutex;

static PROCESS: Mutex<()> = Mutex::new(());

fn ark(args: &[&str]) -> Output {
    let _process = PROCESS.lock().unwrap();
    Command::new(env!("CARGO_BIN_EXE_ark"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

/// Walks the executable command tree through its generated help pages.
fn commands() -> Vec<(Vec<String>, String)> {
    let mut pending = vec![Vec::new()];
    let mut commands = Vec::new();
    while let Some(path) = pending.pop() {
        let mut args: Vec<_> = path.iter().map(String::as_str).collect();
        args.extend(["--help", "--format", "text"]);
        let output = ark(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let page = String::from_utf8(output.stdout).unwrap();
        for line in page
            .lines()
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .take_while(|line| line.starts_with("  "))
        {
            let mut child = path.clone();
            child.push(line.split_whitespace().next().unwrap().to_string());
            pending.push(child);
        }
        commands.push((path, page));
    }
    assert!(commands.len() > 20);
    commands
}

/// Collects JSON leaf paths; scalar arrays remain one field in text.
fn fields<'a>(path: &str, value: &'a Value, fields: &mut BTreeMap<String, &'a Value>) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                fields_insert(path, key, value, fields);
            }
        }
        Value::Array(array)
            if array
                .iter()
                .any(|value| value.is_object() || value.is_array()) =>
        {
            for (index, value) in array.iter().enumerate() {
                fields_insert(path, &index.to_string(), value, fields);
            }
        }
        _ => {
            fields.insert(path.to_string(), value);
        }
    }
}

fn fields_insert<'a>(
    path: &str,
    key: &str,
    value: &'a Value,
    result: &mut BTreeMap<String, &'a Value>,
) {
    fields(
        &if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        },
        value,
        result,
    );
}

fn conformance(args: &[&str]) {
    let mut invocation = args.to_vec();
    invocation.extend(["--format", "json"]);
    let json = ark(&invocation);
    *invocation.last_mut().unwrap() = "text";
    let text = ark(&invocation);
    assert_eq!(text.status.code(), json.status.code(), "{args:?}");
    let document: Value = serde_json::from_slice(&json.stdout).unwrap();
    let mut expected = BTreeMap::new();
    fields("", &document, &mut expected);
    let mut actual = BTreeMap::<String, String>::new();
    let mut key = String::new();
    let output = String::from_utf8(text.stdout).unwrap();
    for line in output.lines() {
        if let Some(continuation) = line.strip_prefix("  ") {
            let value = actual.get_mut(&key).unwrap();
            value.push('\n');
            value.push_str(continuation);
        } else {
            let (name, value) = line.split_once(": ").unwrap();
            key = name.to_string();
            assert!(actual.insert(key.clone(), value.to_string()).is_none());
        }
    }
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "{args:?}: {output}"
    );
    for (key, value) in expected {
        let text = &actual[&key];
        if (key.ends_with("_bytes") || key.ends_with("_seconds")) && !value.is_null() {
            assert_eq!(
                text.parse::<u64>().unwrap(),
                value.as_u64().unwrap(),
                "{args:?}: {key}"
            );
        }
        let expected = match value {
            Value::Null => "-".to_string(),
            Value::Bool(value) => if *value { "yes" } else { "no" }.to_string(),
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        assert_eq!(text, &expected, "{args:?}: {key}");
    }
}

#[test]
fn device_free_results_have_text_json_parity() {
    for (path, page) in commands() {
        let args: Vec<_> = path.iter().map(String::as_str).collect();
        if path.is_empty() {
            conformance(&["--version"]);
        } else if page.contains("Requires: nothing") {
            match args.as_slice() {
                ["help"] => {
                    for topic in [
                        "agents", "states", "output", "devices", "datasets", "apps", "--all",
                    ] {
                        assert_eq!(
                            ark(&["help", topic, "--format", "text"]).stdout,
                            ark(&["help", topic, "--format", "json"]).stdout
                        );
                    }
                }
                ["completions"] => {
                    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
                        assert_eq!(
                            ark(&["completions", shell, "--format", "text"]).stdout,
                            ark(&["completions", shell, "--format", "json"]).stdout
                        );
                    }
                }
                _ => {
                    let mut args = args;
                    // An invalid locator prevents doctor from opening any attached Ark.
                    args.extend(["--device", "hardware:palette-no-device", "--no-input"]);
                    conformance(&args);
                }
            }
        }
    }
}

#[test]
fn help_differs_exactly_where_it_promises_more() {
    for (path, long) in commands() {
        let mut args: Vec<_> = path.iter().map(String::as_str).collect();
        args.extend(["-h", "--format", "text"]);
        let output = ark(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let short = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            short != long,
            short.contains("see more with '--help'"),
            "{path:?}: {short}"
        );
    }
    for args in [["--help", "--all"], ["--all", "--help"], ["-h", "--all"]] {
        let output = ark(&args);
        assert!(output.status.success());
        assert_eq!(output.stdout, ark(&["help", "--all"]).stdout);
    }
    assert_eq!(ark(&["--all"]).status.code(), Some(2));
}

#[test]
fn documented_usage_errors_keep_the_text_prefix() {
    let help = String::from_utf8(ark(&["help", "output"]).stdout).unwrap();
    assert!(help.contains("Exit 2, `usage`"));
    for args in [
        vec!["bogus"],
        vec!["--timeout", "0", "status"],
        vec!["--timeout", "18446744073709551615", "status"],
        vec!["data", "show", "0"],
        vec!["app", "cancel", "invalid"],
        vec!["--version", "status"],
        vec!["help", "no-such-topic"],
    ] {
        for format in ["text", "auto"] {
            let mut invocation = args.clone();
            invocation.extend(["--format", format]);
            let output = ark(&invocation);
            assert_eq!(output.status.code(), Some(2), "{invocation:?}: {output:?}");
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                stderr.starts_with("error[usage]: "),
                "{invocation:?}: {stderr}"
            );
            assert!(!stderr.contains('\x1b'));
        }
    }
    let output = ark(&[
        "status",
        "--device",
        "hardware:palette-no-device",
        "--format",
        "text",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(help.contains("`no-device`"));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("error[no-device]: "))
    );
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
fn explicit_human_pipes_have_layout_without_terminal_escapes() {
    let output = ark(&["--version", "--format", "human"]);
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("  Tool")
    );
    let output = ark(&["--format", "human", "help", "states"]);
    assert!(output.status.success());
    let topic = String::from_utf8(output.stdout).unwrap();
    assert!(!topic.contains('\x1b'));
    assert!(!topic.starts_with('#'));
    assert!(!topic.contains('`'));
}

#[test]
fn format_selection_applies_before_help_and_usage_errors() {
    let text = ark(&["data", "fetch", "--help", "--format", "text"]);
    assert_eq!(
        text.stdout,
        ark(&["--format=text", "data", "fetch", "--help"]).stdout
    );
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .contains("Requires:")
    );
    let human = ark(&["data", "fetch", "--help", "--format=human"]);
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("Requires   ")
    );
    let output = ark(&["--format", "text", "app", "cancel", "invalid"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.contains(&0x1b));
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
    let _process = PROCESS.lock().unwrap();
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
