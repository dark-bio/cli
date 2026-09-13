// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Process-level contracts that require no device or network access.

use serde_json::Value;
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
        args.push("--help");
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

/// Checks stdout indentation and the one-event-per-line stderr contract.
fn json_output(output: &Output) -> Value {
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}\n", serde_json::to_string_pretty(&document).unwrap())
    );
    assert!(!output.stdout.contains(&0x1b));
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        assert!(event["event"].is_string(), "{event}");
    }
    document
}

fn conformance(args: &[&str]) {
    let mut invocation = args.to_vec();
    invocation.push("--json");
    let json = ark(&invocation);
    let human = ark(args);
    assert_eq!(human.status.code(), json.status.code(), "{args:?}");
    json_output(&json);
    let output = String::from_utf8(human.stdout).unwrap();
    assert!(!output.contains('\x1b'));
    for line in output.lines() {
        let label = line.trim_start().split("  ").next().unwrap();
        let label = label.to_ascii_lowercase();
        for suffix in ["_bytes", "_seconds", " bytes", " seconds"] {
            assert!(!label.ends_with(suffix), "{args:?}: {line}");
        }
    }
}

#[test]
fn command_tree_output_conforms() {
    for (path, page) in commands() {
        let args: Vec<_> = path.iter().map(String::as_str).collect();
        assert!(page.contains("--json"), "{path:?}");
        assert!(!page.contains("--format"), "{path:?}");
        assert!(page.contains("--log"), "{path:?}");
        let mut rejected = args.clone();
        rejected.extend(["--json", "--format", "json"]);
        let output = ark(&rejected);
        assert_eq!(output.status.code(), Some(2), "{path:?}");
        assert_eq!(json_output(&output)["error"]["code"], "usage");
        if path.is_empty() {
            conformance(&["--version"]);
        } else if page.contains("Requires: nothing") {
            match args.as_slice() {
                ["help"] => {
                    for topic in [
                        "agents", "states", "output", "devices", "datasets", "apps", "--all",
                    ] {
                        assert_eq!(
                            ark(&["help", topic]).stdout,
                            ark(&["help", topic, "--json"]).stdout
                        );
                    }
                }
                ["completions"] => {
                    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
                        assert_eq!(
                            ark(&["completions", shell]).stdout,
                            ark(&["completions", shell, "--json"]).stdout
                        );
                    }
                }
                _ => {
                    let mut args = args;
                    // Never open an attached Ark during conformance tests.
                    args.extend(["--device", "hardware:palette-no-device", "--no-input", "-v"]);
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
        args.push("-h");
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
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with("error[usage]: "), "{args:?}: {stderr}");
        assert!(!stderr.contains('\x1b'));
    }
    let output = ark(&["status", "--device", "hardware:palette-no-device"]);
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
        vec!["data", "fetch", "--all", "reference-genome", "--json"],
        vec!["--json", "data", "upload", "x", "--dry-run", "--unlock"],
        vec!["--json", "--timeout", "0", "status"],
        vec!["--json", "app", "cancel", "18446744073709551616"],
        vec!["--json", "--version", "status"],
        vec!["--json", "help", "missing"],
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
    let output = ark(&["--version", "--json"]);
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
fn default_pipes_have_layout_without_terminal_escapes() {
    let output = ark(&["--version"]);
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("  Tool")
    );
}

#[test]
fn pipes_cannot_force_color() {
    let _process = PROCESS.lock().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ark"))
        .args(["--version"])
        .env_remove("NO_COLOR")
        .env("CLICOLOR", "1")
        .env("CLICOLOR_FORCE", "1")
        .env("FORCE_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}

#[test]
fn json_selection_applies_before_help_and_usage_errors() {
    let help = ark(&["data", "fetch", "--help", "--json"]);
    assert_eq!(
        help.stdout,
        ark(&["--json", "data", "fetch", "--help"]).stdout
    );
    assert_eq!(help.stdout, ark(&["data", "fetch", "--help"]).stdout);
    for flag in [
        "--format",
        "--format=json",
        "--format=text",
        "--format=auto",
        "--format=human",
        "-vv",
        "-vvv",
    ] {
        let output = ark(&[flag]);
        assert_eq!(output.status.code(), Some(2), "{flag}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("error[usage]: "));
    }
}

#[test]
fn help_matches_the_supported_palette() {
    let output = ark(&["--help"]);
    assert!(output.status.success());
    let root = String::from_utf8(output.stdout).unwrap();
    assert!(root.lines().count() <= 42, "{root}");
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
        assert!(long.contains("--timeout <SECONDS>"));
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
