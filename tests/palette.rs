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
        .env("CI", "1")
        .output()
        .unwrap()
}

/// The private update entry point does nothing and prints nothing under CI.
#[test]
fn test_update_entry_point_is_silent_under_ci() {
    let output = ark(&["__update"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// Stamps a kept update answer as asked now. The spawned ark judges the
/// answer's age against the real wall time, so the stamp reads it too.
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the spawned binary compares the answer's stamp with the real wall time"
)]
fn asked_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A fresh isolated answer produces one stderr note while help and invalid invocations stay quiet.
#[cfg(unix)]
#[test]
fn test_update_note_preserves_command_output_and_excludes_noncommands() {
    /// Removes the subprocess home and cache even after an assertion failure.
    struct Directory(
        /// Isolated root used for both HOME and XDG_CACHE_HOME.
        std::path::PathBuf,
    );
    impl Drop for Directory {
        /// Cleans up files owned by this process test.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // macOS uses Library/Caches while other Unix targets use XDG_CACHE_HOME
    let _process = PROCESS.lock().unwrap();
    let directory =
        Directory(std::env::temp_dir().join(format!("ark-update-palette-{}", std::process::id())));
    let home = directory.0.join("home");
    let xdg_cache = directory.0.join("cache");
    let cache = if cfg!(target_os = "macos") {
        home.join("Library/Caches/ark")
    } else {
        xdg_cache.join("ark")
    };
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let version = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let newest = format!("{}.0.0", version.major + 1);
    let answer = serde_json::to_vec(&serde_json::json!({
        "channel": if version.pre.is_empty() { "release" } else { "develop" },
        "asked": asked_now(),
        "newest": newest,
    }))
    .unwrap();
    std::fs::write(cache.join("update.json"), &answer).unwrap();
    let invoke = |args: &[&str], ci: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ark"));
        command
            .args(args)
            .env_remove("CI")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", &xdg_cache)
            .env("NO_COLOR", "1");
        if let Some(ci) = ci {
            command.env("CI", ci);
        }
        command.output().unwrap()
    };

    // The note precedes the error and repeats in both reading and JSON output
    let message = format!(
        "ark {newest} is available, this is {version}; download it from https://github.com/dark-bio/cli"
    );
    for json in [false, true] {
        let mut args = vec!["status", "--device", "hardware:palette-no-device"];
        if json {
            args.push("--json");
        }
        let baseline = invoke(&args, Some("1"));
        for ci in [None, Some("")] {
            let output = invoke(&args, ci);
            assert_eq!(output.status.code(), Some(3), "json={json}, CI={ci:?}");
            assert_eq!(output.stdout, baseline.stdout, "json={json}, CI={ci:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            if json {
                let events: Vec<Value> = stderr
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    events[0],
                    serde_json::json!({"event":"note","message":message})
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["event"] == "note")
                        .count(),
                    1
                );
                assert_eq!(events[1]["event"], "error");
            } else {
                assert_eq!(stderr.lines().next().unwrap(), format!("note: {message}"));
                assert_eq!(
                    stderr
                        .lines()
                        .filter(|line| line.starts_with("note:"))
                        .count(),
                    1
                );
            }
        }

        // Quiet suppresses the notice without changing the result or status
        args.push("-q");
        let quiet = invoke(&args, None);
        assert_eq!(quiet.stdout, baseline.stdout);
        assert_eq!(quiet.status.code(), baseline.status.code());
        assert!(!String::from_utf8_lossy(&quiet.stderr).contains("is available"));
        assert!(!String::from_utf8_lossy(&baseline.stderr).contains("is available"));
    }

    // None of these paths may announce or refresh a release, even with a known newer build
    for args in [
        vec!["help"],
        vec!["help", "--all"],
        vec!["-h"],
        vec!["--help"],
        vec!["--help", "--all"],
        vec!["status", "--help"],
        vec!["completions", "zsh"],
        vec!["--version"],
        vec![],
        vec!["--all"],
        vec!["--timeout", "0", "status"],
        vec!["bogus"],
        vec!["--json", "data", "upload", "x", "--dry-run", "--unlock"],
        vec!["--version", "status"],
    ] {
        let output = invoke(&args, None);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("is available"),
            "{args:?}"
        );
    }
    assert_eq!(std::fs::read(cache.join("update.json")).unwrap(), answer);
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
        // The shared options are listed once, on the root page. Examples
        // mention the flags too, so the test identifies the listing by its
        // description.
        for option in ["--timeout <SECONDS>", "Print the complete result as JSON"] {
            assert_eq!(page.contains(option), path.is_empty(), "{path:?}: {option}");
        }
        assert!(!page.contains("--format"), "{path:?}");
        assert!(page.contains("-h, --help"), "{path:?}");
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
    // Topics render in a pipe as on a terminal, so code spans lose their
    // backtick markers there and keep only their text.
    let help = String::from_utf8(ark(&["help", "output"]).stdout).unwrap();
    assert!(help.contains("Exit 2, usage"));
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
    assert!(help.contains("no-device: no Ark found"));
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
        // The contract block keeps one shape in a pipe: colon labels, wrapped
        // values, and examples as bare commands with no prompt.
        for line in long.lines() {
            assert!(line.chars().count() <= 80, "{path}: {line}");
            assert!(!line.trim_start().starts_with("$ "), "{path}: {line}");
        }
        assert!(!long.contains("--timeout <SECONDS>"), "{path}");
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

/// The manual names the example apps and the emulator, the two other corners
/// of the loop a reader arrives in, and the root page lists the shared options.
#[test]
fn manual_carries_the_cross_references() {
    // Wrapped, so a phrase is looked for across line breaks.
    let manual = String::from_utf8(ark(&["help", "--all"]).stdout).unwrap();
    let manual = manual.split_whitespace().collect::<Vec<_>>().join(" ");
    for link in [
        "https://github.com/dark-bio/examples",
        "https://github.com/dark-bio/emulator",
        "ark-emulator help agents",
    ] {
        assert!(manual.contains(link), "{link}");
    }
    let root = String::from_utf8(ark(&["--help"]).stdout).unwrap();
    assert!(root.contains("--timeout <SECONDS>"));
    assert!(root.contains("--json"));
}

/// Every error code the source can emit, read from the source itself, so the
/// output topic is checked against what the tool does and not a second list.
fn emitted_codes() -> std::collections::BTreeSet<String> {
    fn visit(dir: &std::path::Path, codes: &mut std::collections::BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, codes);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for prefix in ["Error::new(", "Self::new("] {
                for (index, _) in text.match_indices(prefix) {
                    let rest = text[index + prefix.len()..]
                        .trim_start()
                        .trim_start_matches(|c: char| c.is_ascii_digit())
                        .trim_start()
                        .trim_start_matches(',')
                        .trim_start();
                    if let Some(rest) = rest.strip_prefix('"')
                        && let Some(end) = rest.find('"')
                    {
                        codes.insert(rest[..end].to_string());
                    }
                }
            }
            // The Ark's reserved verdicts map to codes in match arms.
            if path.file_name().is_some_and(|name| name == "error.rs") {
                for (index, _) in text.match_indices("=> \"") {
                    let rest = &text[index + 4..];
                    if let Some(end) = rest.find('"') {
                        codes.insert(rest[..end].to_string());
                    }
                }
            }
        }
    }
    let mut codes = std::collections::BTreeSet::new();
    visit(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut codes,
    );
    assert!(codes.len() > 20, "{codes:?}");
    codes
}

#[test]
fn every_error_code_is_documented() {
    let topic = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/help/output.md"),
    )
    .unwrap();
    for code in emitted_codes() {
        assert!(
            topic.contains(&format!("`{code}`")),
            "{code} is not in `ark help output`"
        );
    }
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
