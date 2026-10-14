// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Process-level contracts that require no device or network access.

use serde_json::Value;
use std::process::{Command, Output};
use std::sync::Mutex;

/// Lock that keeps the tests' `ark` processes from running at the same time.
static PROCESS: Mutex<()> = Mutex::new(());

/// Public topics in the order the root page advertises them.
const TOPICS: [&str; 7] = [
    "agents", "devices", "datasets", "apps", "firmware", "output", "errors",
];

/// Runs the `ark` binary with `args` under the process lock, without color and
/// with release lookups off.
fn ark(args: &[&str]) -> Output {
    let _process = PROCESS.lock().unwrap();
    Command::new(env!("CARGO_BIN_EXE_ark"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .output()
        .unwrap()
}

/// Checks that the population list needs no Ark, whatever the device options.
#[test]
fn test_samples_list_populations_without_an_ark() {
    for args in [
        vec!["data", "samples"],
        vec![
            "--device",
            "emulator:0",
            "--unlock",
            "--env",
            "develop",
            "data",
            "samples",
        ],
        vec![
            "data",
            "samples",
            "--device",
            "invalid selector",
            "--unlock",
            "--no-input",
        ],
    ] {
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {:?}", output.stderr);
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 27, "{args:?}");
        assert_eq!(
            text.lines().take(3).collect::<Vec<_>>(),
            [
                "POPULATION  SAMPLES  DESCRIPTION",
                "ACB             116  African Caribbean in Barbados",
                "ASW              74  African Ancestry in Southwest US",
            ],
            "{args:?}"
        );
        assert!(!text.contains('\x1b'), "pipe output has no ANSI escapes");
    }

    // The JSON document holds every population with its count and description
    let output = ark(&["data", "samples", "--json"]);
    assert_eq!(output.status.code(), Some(0), "population JSON succeeds");
    assert!(
        output.stderr.is_empty(),
        "population JSON is silent on stderr"
    );
    let document = json_output(&output);
    assert_eq!(
        document.as_object().unwrap().len(),
        1,
        "population document fields"
    );
    let populations = document["populations"].as_array().unwrap();
    assert_eq!(populations.len(), 26, "population JSON count");
    assert_eq!(
        populations[0],
        serde_json::json!({
            "population": "ACB", "count": 116, "description": "African Caribbean in Barbados",
        }),
        "first population JSON row"
    );
    assert_eq!(
        populations
            .iter()
            .map(|row| row["count"].as_u64().unwrap())
            .sum::<u64>(),
        3_202,
        "total cohort count"
    );
    assert!(
        populations
            .windows(2)
            .all(|pair| pair[0]["population"].as_str() < pair[1]["population"].as_str()),
        "population code order"
    );
}

/// Checks the people listings and their JSON, by population, sex or neither.
#[test]
fn test_samples_views_and_json() {
    for (args, count, columns) in [
        (
            vec!["data", "samples", "JPT"],
            104,
            vec!["NAME", "SAMPLE", "SEX"],
        ),
        (
            vec!["data", "samples", "JPT", "--sex", "female"],
            48,
            vec!["NAME", "SAMPLE", "SEX"],
        ),
        (
            vec!["data", "samples", "JPT", "--sex", "male"],
            56,
            vec!["NAME", "SAMPLE", "SEX"],
        ),
        (
            vec!["data", "samples", "--all"],
            3_202,
            vec!["NAME", "SAMPLE", "POPULATION", "SEX"],
        ),
    ] {
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), count + 1, "{args:?}");
        assert_eq!(
            text.lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            columns,
            "{args:?}"
        );

        // The JSON holds every field, including those the table leaves out
        let mut invocation = args.clone();
        invocation.push("--json");
        let output = ark(&invocation);
        assert_eq!(output.status.code(), Some(0), "{invocation:?}");
        assert!(output.stderr.is_empty(), "{invocation:?}");
        let document = json_output(&output);
        assert_eq!(document.as_object().unwrap().len(), 1, "{invocation:?}");
        let samples = document["samples"].as_array().unwrap();
        assert_eq!(samples.len(), count, "{invocation:?}");
        assert!(
            samples
                .windows(2)
                .all(|pair| pair[0]["sample"].as_str() < pair[1]["sample"].as_str()),
            "sample order: {invocation:?}"
        );
        for sample in samples {
            assert_eq!(
                sample
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["name", "sample", "population", "sex", "father", "mother"],
                "{sample}"
            );
            assert!(
                matches!(sample["sex"].as_str(), Some("male" | "female")),
                "{sample}"
            );
            if args.contains(&"JPT") {
                assert_eq!(sample["population"], "JPT", "{sample}");
            }
            if args.contains(&"--sex") {
                assert_eq!(sample["sex"], *args.last().unwrap(), "{sample}");
            }
        }
        if args.contains(&"--all") {
            assert!(
                samples
                    .iter()
                    .find(|sample| sample["sample"] == "HG00155")
                    .unwrap()["mother"]
                    .is_null(),
                "parents outside the cohort are null"
            );
            assert_eq!(
                samples[0],
                serde_json::json!({
                    "name": "orange-ravine", "sample": "HG00096", "population": "GBR",
                    "sex": "male", "father": null, "mother": null,
                }),
                "first sample JSON row"
            );
            assert_eq!(
                samples
                    .iter()
                    .find(|sample| sample["sample"] == "HG00405")
                    .unwrap(),
                &serde_json::json!({
                    "name": "lemon-grass", "sample": "HG00405", "population": "CHS",
                    "sex": "female", "father": "HG00403", "mother": "HG00404",
                }),
                "sample JSON parents"
            );
        }
    }

    // The sex filter applies across all populations too
    let output = ark(&["data", "samples", "--all", "--sex", "female", "--json"]);
    assert_eq!(output.status.code(), Some(0), "all-population sex filter");
    let document = json_output(&output);
    let samples = document["samples"].as_array().unwrap();
    assert!(!samples.is_empty(), "female samples exist");
    assert!(
        samples.iter().all(|sample| sample["sex"] == "female"),
        "all-population filter selects females"
    );
}

/// Checks where samples sits in the data group, its help and its completion.
#[test]
fn test_samples_help_and_completion() {
    let output = ark(&["data", "-h"]);
    assert_eq!(output.status.code(), Some(0), "data help succeeds");
    let text = String::from_utf8(output.stdout).unwrap();
    let commands = text
        .split_once("Commands:\n")
        .unwrap()
        .1
        .split_once("\nOptions:")
        .unwrap()
        .0;
    assert_eq!(
        commands
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect::<Vec<_>>(),
        [
            "paths", "list", "show", "upload", "fetch", "samples", "delete", "repair"
        ],
        "data command order"
    );
    let help = ark(&["data", "samples", "-h"]);
    assert_eq!(help.status.code(), Some(0), "samples help succeeds");
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(
        text.contains("Usage: ark data samples [OPTIONS] [POPULATION]"),
        "samples usage"
    );
    assert!(
        !text.contains("ACB") && !text.contains("YRI"),
        "help omits population value lists"
    );
    assert!(
        !text.contains("possible values:"),
        "help omits the sex value list"
    );

    // The zsh script describes each population, while help leaves the codes out
    let output = ark(&["completions", "zsh"]);
    assert_eq!(output.status.code(), Some(0), "zsh completion succeeds");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("JPT\\:\"Japanese in Tokyo, Japan\""),
        "JPT description in zsh completion"
    );
    for code in [
        "ACB", "ASW", "BEB", "CDX", "CEU", "CHB", "CHS", "CLM", "ESN", "FIN", "GBR", "GIH", "GWD",
        "IBS", "ITU", "JPT", "KHV", "LWK", "MSL", "MXL", "PEL", "PJL", "PUR", "STU", "TSI", "YRI",
    ] {
        assert!(
            text.contains(&format!("{code}\\:\"")),
            "described completion for {code}"
        );
    }
    assert!(
        text.contains("male") && text.contains("female"),
        "sex completion values"
    );
    assert!(
        text.contains("forked-knoll\\:\"NA12878 · CEU · female\""),
        "sample description in zsh completion"
    );

    // The fetch page describes --sample without listing the cohort
    let output = ark(&["data", "fetch", "--help"]);
    assert_eq!(output.status.code(), Some(0), "fetch help succeeds");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("Download public data onto the Ark\n"),
        "fetch one-liner"
    );
    assert!(
        text.contains("--sample <NAME>"),
        "sample option placeholder"
    );
    assert!(
        text.contains("Fill the slot with a public genome, by name or sample ID"),
        "sample option help"
    );
    assert!(
        !text.contains("possible values:"),
        "help omits sample value lists"
    );
    assert!(
        !text.contains("orange-ravine"),
        "help omits catalog names beyond its examples"
    );
}

/// Checks that every help route reports a failed stdout write as local I/O,
/// retaining text output and the selected error-event format.
#[cfg(unix)]
#[test]
fn test_help_write_failures_return_local_errors() {
    let _process = PROCESS.lock().unwrap();
    for args in [
        &[][..],
        &["-h"],
        &["--help"],
        &["--help", "--all"],
        &["help"],
        &["help", "--all"],
        &["help", "agents"],
        &["help", "data", "upload"],
        &["data", "upload", "-h"],
        &["data", "upload", "--help"],
    ] {
        for json in [false, true] {
            // Close the reader before starting the child, so the first write
            // fails without depending on another process's scheduling
            let (reader, stdout) = std::os::unix::net::UnixStream::pair().unwrap();
            drop(reader);
            let stdout: std::os::fd::OwnedFd = stdout.into();
            let mut command = Command::new(env!("CARGO_BIN_EXE_ark"));
            command.args(args).env("NO_COLOR", "1").env("CI", "1");
            if json {
                command.arg("--json");
            }
            let output = command.stdout(stdout).output().unwrap();
            assert_eq!(output.status.code(), Some(1), "{args:?}, json={json}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            if json {
                let event: Value = serde_json::from_str(&stderr).unwrap();
                assert_eq!(event["event"], "error", "{args:?}");
                assert_eq!(event["error"]["code"], "io", "{args:?}");
            } else {
                assert!(stderr.starts_with("error[io]:"), "{args:?}: {stderr}");
            }
        }
    }
}

/// Checks that the private update entry point does nothing and prints nothing
/// under CI.
#[test]
fn test_update_entry_point_is_silent_under_ci() {
    let output = ark(&["__update"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// Local builds skip both the release note and the background lookup, even
/// when the kept answer is stale.
#[cfg(all(unix, ark_local))]
#[test]
fn test_local_build_skips_release_notes_and_lookup() {
    /// Home and cache of the spawned processes, removed even after an assertion
    /// failure.
    struct Directory(
        /// Isolated root holding the `HOME` and `XDG_CACHE_HOME` directories.
        std::path::PathBuf,
    );

    impl Drop for Directory {
        /// Removes the whole isolated root.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Keep an answer that names the next major version in an isolated cache
    let _process = PROCESS.lock().unwrap();
    let directory =
        Directory(std::env::temp_dir().join(format!("ark-update-palette-{}", std::process::id())));
    let home = directory.0.join("home");
    let xdg_cache = directory.0.join("cache");
    // macOS uses Library/Caches while other Unix targets use XDG_CACHE_HOME
    let cache = if cfg!(target_os = "macos") {
        home.join("Library/Caches/ark")
    } else {
        xdg_cache.join("ark")
    };
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let version = semver::Version::parse(env!("ARK_BUILD_VERSION")).unwrap();
    let newest = format!("{}.0.0", version.major + 1);
    let answer = serde_json::to_vec(&serde_json::json!({
        "channel": if version.pre.is_empty() { "release" } else { "develop" },
        "asked": "2000-01-01T00:00:00Z",
        "newest": newest,
    }))
    .unwrap();
    std::fs::write(cache.join("update.json"), &answer).unwrap();

    // Invocations run in the isolated home, with CI set only when asked
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

    // Local builds keep the baseline output and leave the stale cache untouched
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
            assert_eq!(output.stderr, baseline.stderr, "json={json}, CI={ci:?}");
        }

        // Quiet keeps the result, still without a release note
        args.push("-q");
        let quiet = invoke(&args, None);
        assert_eq!(quiet.stdout, baseline.stdout);
        assert_eq!(quiet.status.code(), baseline.status.code());
        assert!(!String::from_utf8_lossy(&quiet.stderr).contains("is available"));
        assert!(!String::from_utf8_lossy(&baseline.stderr).contains("is available"));
    }

    assert_eq!(std::fs::read(cache.join("update.json")).unwrap(), answer);

    // A local build also does nothing when the lookup copy starts directly, and
    // never fills an empty cache
    let output = invoke(&["__update"], None);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read(cache.join("update.json")).unwrap(), answer);
    std::fs::remove_file(cache.join("update.json")).unwrap();
    let output = invoke(&["status", "--device", "hardware:palette-no-device"], None);
    assert_eq!(output.status.code(), Some(3));
    assert!(!cache.join("update.json").exists());
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
        let children: Vec<_> = page
            .lines()
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .take_while(|line| line.is_empty() || line.starts_with("  "))
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut child = path.clone();
                child.push(line.split_whitespace().next().unwrap().to_string());
                child
            })
            .collect();
        pending.extend(children.into_iter().rev());
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

/// Checks that an invocation exits alike with and without `--json`, and both
/// outputs keep their format contracts.
///
/// Human output carries no escape sequences, and no label ends in a unit suffix
/// such as `_bytes` or `_seconds`.
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

/// Checks that every command's help page and output follow the shared
/// conventions.
#[test]
fn command_tree_output_conforms() {
    for (path, page) in commands() {
        let args: Vec<_> = path.iter().map(String::as_str).collect();
        // The shared options are listed once, on the root page. The leading
        // spaces distinguish the JSON option's row from examples and prose.
        for option in ["--timeout <SECONDS>", "      --json "] {
            assert_eq!(page.contains(option), path.is_empty(), "{path:?}: {option}");
        }
        assert!(!page.contains("--format"), "{path:?}");
        assert!(page.contains("-h, --help"), "{path:?}");

        // Every command rejects --format as a usage error, reported in JSON
        let mut rejected = args.clone();
        rejected.extend(["--json", "--format", "json"]);
        let output = ark(&rejected);
        assert_eq!(output.status.code(), Some(2), "{path:?}");
        assert_eq!(json_output(&output)["error"]["code"], "usage");

        // The root and every command that requires nothing run in both formats
        if path.is_empty() {
            conformance(&["--version"]);
        } else if page.contains("Requires  nothing") {
            match args.as_slice() {
                ["help"] => {
                    for topic in TOPICS.into_iter().chain(["--all"]) {
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
                    // Never open an attached Ark during conformance tests
                    args.extend(["--device", "hardware:palette-no-device", "--no-input", "-v"]);
                    conformance(&args);
                }
            }
        }
    }
}

/// Checks paragraph placement, every public topic and the full manual's page
/// order through the executable.
#[test]
fn test_paragraphs_and_topics_follow_the_manual_order() {
    // Every long command page opens with a description and a nonempty paragraph
    let commands = commands();
    for (path, long) in &commands {
        let (opening, _) = long.split_once("\n\nUsage:").unwrap();
        let (description, paragraph) = opening.split_once("\n\n").unwrap();
        assert!(!description.is_empty(), "{path:?}");
        assert!(!paragraph.trim().is_empty(), "{path:?}");
        assert_eq!(paragraph, paragraph.trim(), "{path:?}");

        let mut args: Vec<_> = path.iter().map(String::as_str).collect();
        args.push("-h");
        let output = ark(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let short = String::from_utf8(output.stdout).unwrap();
        assert_eq!(&short, long, "{path:?}");

        // A single name shared with a topic prints the topic
        if path.len() != 1 || !TOPICS.contains(&path[0].as_str()) {
            let mut args = vec!["help"];
            args.extend(path.iter().map(String::as_str));
            let output = ark(&args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert_eq!(output.stdout, long.as_bytes(), "{path:?}");
        }
    }

    // The root lists all topics, and each prints text even in JSON mode
    let closing = format!("Topics: {}.", TOPICS.join(", "));
    assert!(commands[0].1.contains(&closing));
    let topics: Vec<_> = TOPICS
        .iter()
        .map(|name| {
            let output = ark(&["help", name]);
            assert!(output.status.success(), "{name}: {output:?}");
            assert!(output.stderr.is_empty(), "{name}");
            assert_eq!(output.stdout, ark(&["help", name, "--json"]).stdout);
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(!text.trim().is_empty(), "{name}");
            text
        })
        .collect();

    // Root, agents, command tree, then the remaining topics are separate pages
    let output = ark(&["help", "--all"]);
    assert!(output.status.success(), "{output:?}");
    let manual = String::from_utf8(output.stdout).unwrap();
    let separator = format!("\n\n{}\n\n", "-".repeat(80));
    let pages: Vec<_> = manual.trim_end().split(&separator).collect();
    let mut expected = vec![commands[0].1.trim_end(), topics[0].trim_end()];
    expected.extend(commands[1..].iter().map(|(_, page)| page.trim_end()));
    expected.extend(topics[1..].iter().map(|page| page.trim_end()));
    assert_eq!(pages.len(), expected.len());
    for (index, (page, expected)) in pages.iter().zip(expected).enumerate() {
        assert_eq!(*page, expected, "page {index}");
    }
}

/// Checks that a name shared with a topic prints the topic, while its command
/// page stays reachable.
#[test]
fn test_topics_take_precedence_over_single_command_names() {
    for (name, heading) in [("devices", "Devices"), ("firmware", "Firmware")] {
        let topic = ark(&["help", name]);
        assert!(topic.status.success(), "{name}");
        let text = String::from_utf8(topic.stdout).unwrap();
        assert!(text.starts_with(&format!("{heading}\n\n")), "{name}");
        assert!(!text.contains("Usage:"), "{name}");

        let command = ark(&[name, "--help"]);
        assert!(command.status.success(), "{name}");
        assert!(
            String::from_utf8_lossy(&command.stdout).contains("Usage:"),
            "{name}"
        );
        assert_eq!(command.stdout, ark(&[name, "-h"]).stdout, "{name}");
    }

    // Longer help paths still walk the command tree
    let nested = ark(&["help", "firmware", "list"]);
    assert!(nested.status.success());
    assert_eq!(nested.stdout, ark(&["firmware", "list", "--help"]).stdout);
}

/// Checks that every help spelling prints the whole page and the manual stays text.
#[test]
fn test_help_spellings_print_the_same_page() {
    for (path, long) in commands() {
        let mut args: Vec<_> = path.iter().map(String::as_str).collect();
        args.push("-h");
        let output = ark(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let short = String::from_utf8(output.stdout).unwrap();
        assert_eq!(short, long, "{path:?}");
        assert!(!short.contains("see more"), "{path:?}");
    }

    // Every spelling of the manual prints it, and --all alone is a usage error
    for args in [["--help", "--all"], ["--all", "--help"], ["-h", "--all"]] {
        let output = ark(&args);
        assert!(output.status.success());
        assert_eq!(output.stdout, ark(&["help", "--all"]).stdout);
    }
    assert_eq!(ark(&["--all"]).status.code(), Some(2));
}

/// Checks the root's command groups, option order, exit classes and closing lines.
#[test]
fn test_root_help_groups_commands_and_lists_shared_exits() {
    let output = ark(&["--help"]);
    let page = String::from_utf8(output.stdout).unwrap();
    let (_, commands) = page.split_once("Commands:\n").unwrap();
    let (commands, options) = commands.split_once("\n\nOptions:\n").unwrap();
    let groups: Vec<Vec<_>> = commands
        .split("\n\n")
        .map(|group| {
            group
                .lines()
                .map(|line| line.split_whitespace().next().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        groups,
        [
            vec!["devices", "status", "enroll", "genuine", "pair", "unlock"],
            vec!["data", "run"],
            vec!["firmware", "doctor", "completions", "help"],
        ]
    );

    // The options form one section, in order, followed by the exit codes
    let (options, exits) = options.split_once("\n\nExit codes:\n").unwrap();
    let flags: Vec<_> = options
        .lines()
        .filter_map(|line| line.split_whitespace().find(|word| word.starts_with("--")))
        .collect();
    assert_eq!(
        flags,
        [
            "--device",
            "--json",
            "--unlock",
            "--no-input",
            "--timeout",
            "--env",
            "--quiet",
            "--verbose",
            "--log",
            "--version",
            "--help"
        ]
    );
    for text in [
        "Print JSON, with events as JSON Lines on stderr",
        "Limit each wait on a machine",
        "Hide progress, notes, warnings and steps",
        "Show steps",
        "Print diagnostics: debug or trace",
        "Print the version",
        "Print help; `ark help --all` prints the manual",
    ] {
        assert!(options.contains(text), "{text}");
    }
    let (exits, closing) = exits.split_once("\n\n").unwrap();
    let rows: Vec<_> = exits
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        rows,
        [
            "0 done 6 an approval that did not come",
            "1 a local input or a missing confirmation 7 a machine wait that ran out",
            "2 usage 8 an app failure",
            "3 reaching the Ark 130 Ctrl-C",
            "4 the cloud 143 SIGTERM",
            "5 the Ark's state or refusal",
        ]
    );
    assert_eq!(
        closing,
        "Output is formatted for reading; --json keeps complete, exact values.\nAI agents: read `ark help agents` first.\nTopics: agents, devices, datasets, apps, firmware, output, errors.\n"
    );
}

/// Checks each usage failure's words and suggestions in text and JSON.
#[test]
fn test_usage_errors_name_inputs_and_preserve_suggestions() {
    for (args, message, hints) in [
        (
            vec!["statsu"],
            "unknown command 'statsu'",
            vec!["a similar command exists: `ark status`"],
        ),
        (
            vec!["app"],
            "unknown command 'app'",
            vec!["see `ark --help`"],
        ),
        (
            vec!["app", "run", "app.wasm"],
            "unknown command 'app'",
            vec!["see `ark --help`"],
        ),
        (
            vec!["run", "app.wasm", "--dry-run"],
            "unknown option '--dry-run'",
            vec!["see `ark run --help`"],
        ),
        (
            vec!["firmware", "update", "--version", "2.9.0-bbbbbbb"],
            "unknown option '--version'",
            vec!["a similar option exists: `--verbose`"],
        ),
        (
            vec!["firmware", "update", "-V"],
            "unknown option '-V'",
            vec!["see `ark firmware update --help`"],
        ),
        (
            vec!["data", "uplod"],
            "unknown command 'uplod'",
            vec!["a similar command exists: `ark data upload`"],
        ),
        (
            vec!["--jsno"],
            "unknown option '--jsno'",
            vec!["a similar option exists: `--json`"],
        ),
        (
            vec!["data", "upload", "--dry-rnu"],
            "unknown option '--dry-rnu'",
            vec!["a similar option exists: `--dry-run`"],
        ),
        (
            vec!["--log", "debgu"],
            "unknown value 'debgu' for --log",
            vec!["a similar value exists: `debug`"],
        ),
        (
            vec!["--env", "releas"],
            "unknown environment 'releas'",
            vec!["a similar value exists: `release`"],
        ),
        (
            vec!["--env", "stagin"],
            "unknown environment 'stagin'",
            vec!["a similar value exists: `staging`"],
        ),
        (
            vec!["--env", "develo"],
            "unknown environment 'develo'",
            vec!["a similar value exists: `develop`"],
        ),
        (
            vec!["completions", "bsah"],
            "unknown shell 'bsah'",
            vec!["a similar value exists: `bash`"],
        ),
        (
            vec!["data", "show", "bogus"],
            "unknown slot 'bogus'",
            vec!["use a slot name or id from `ark data list`"],
        ),
        (
            vec!["data", "fetch", "referense-genome"],
            "unknown slot 'referense-genome'",
            vec![
                "a similar value exists: `reference-genome`",
                "use a slot name or id from `ark data list`",
            ],
        ),
        (
            vec!["data", "fetch", "reference-genome", "--all"],
            "[SLOT] cannot be combined with --all",
            vec!["see `ark data fetch --help`"],
        ),
        (
            vec!["data", "fetch", "--all", "reference-genome"],
            "--all cannot be combined with [SLOT]",
            vec!["see `ark data fetch --help`"],
        ),
        (
            vec!["data", "fetch", "--all", "--no-cache", "--cache", "unused"],
            "--no-cache cannot be combined with --cache",
            vec!["see `ark data fetch --help`"],
        ),
        (
            vec!["data", "fetch", "--all", "--cache", "unused", "--no-cache"],
            "--cache cannot be combined with --no-cache",
            vec!["see `ark data fetch --help`"],
        ),
        (
            vec!["data", "samples", "JTP"],
            "unknown population 'JTP'",
            vec![
                "a similar value exists: `JPT`",
                "use a population code from `ark data samples`",
            ],
        ),
        (
            vec![
                "data",
                "fetch",
                "snp-indel-calls",
                "--sample",
                "orange-ravin",
            ],
            "unknown sample 'orange-ravin'",
            vec![
                "a similar value exists: `orange-ravine`",
                "use a sample name or id from `ark data samples`",
            ],
        ),
        (
            vec!["data", "fetch", "snp-indel-calls", "--sample", "????"],
            "unknown sample '????'",
            vec!["use a sample name or id from `ark data samples`"],
        ),
        (
            vec!["data", "fetch", "reference-genome", "--sample", "HG00096"],
            "--sample fills snp-indel-calls, not reference-genome",
            vec!["run `ark data fetch reference-genome`"],
        ),
        (
            vec!["data", "fetch", "1", "--sample", "orange-ravine"],
            "--sample fills snp-indel-calls, not reference-genome",
            vec!["run `ark data fetch reference-genome`"],
        ),
        (
            vec!["data", "fetch", "42", "--sample", "orange-ravine"],
            "--sample fills snp-indel-calls, not 42",
            vec![],
        ),
        (
            vec!["data", "samples", "unknown"],
            "unknown population 'unknown'",
            vec!["use a population code from `ark data samples`"],
        ),
        (
            vec!["data", "samples", "--sex", "female"],
            "--sex needs a population or --all",
            vec!["see `ark data samples --help`"],
        ),
        (
            vec!["data", "samples", "JPT", "--all"],
            "[POPULATION] cannot be combined with --all",
            vec!["see `ark data samples --help`"],
        ),
        (
            vec!["data", "samples", "--all", "JPT"],
            "[POPULATION] cannot be combined with --all",
            vec!["see `ark data samples --help`"],
        ),
        (
            vec!["data", "show", "snp-indel-call"],
            "unknown slot 'snp-indel-call'",
            vec![
                "a similar value exists: `snp-indel-calls`",
                "use a slot name or id from `ark data list`",
            ],
        ),
        (
            vec!["data", "show", "reference-genom"],
            "unknown slot 'reference-genom'",
            vec![
                "a similar value exists: `reference-genome`",
                "use a slot name or id from `ark data list`",
            ],
        ),
        (
            vec!["data", "upload", "calls.vcf", "--slot", "gene-annotation"],
            "unknown slot 'gene-annotation'",
            vec![
                "a similar value exists: `gene-annotations`",
                "use a slot name or id from `ark data list`",
            ],
        ),
        (
            vec!["data", "show", "variant-catalo"],
            "unknown slot 'variant-catalo'",
            vec![
                "a similar value exists: `variant-catalog`",
                "use a slot name or id from `ark data list`",
            ],
        ),
        (
            vec!["help", "agnets"],
            "unknown help topic 'agnets'",
            vec!["a similar topic exists: `ark help agents`"],
        ),
        (
            vec!["help", "data", "uplod"],
            "unknown help command 'uplod'",
            vec!["a similar command exists: `ark data upload`"],
        ),
        (
            vec!["help", "no-such-topic"],
            "unknown help topic 'no-such-topic'",
            vec!["see `ark help --help`"],
        ),
        (
            vec!["data", "upload", "--no-such-option"],
            "unknown option '--no-such-option'",
            vec!["see `ark data upload --help`"],
        ),
    ] {
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let expected = format!(
            "error[usage]: {message}\n{}",
            hints
                .iter()
                .map(|hint| format!("hint: {hint}\n"))
                .collect::<String>()
        );
        assert_eq!(stderr, expected, "{args:?}");
        assert!(!stderr.contains("\\n"), "{args:?}");

        // JSON carries the same message and hints on both streams
        let mut json_args = vec!["--json"];
        json_args.extend(&args);
        let output = ark(&json_args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let document = json_output(&output);
        assert_eq!(document["error"]["message"], message, "{args:?}");
        assert_eq!(
            document["error"]["hints"],
            serde_json::json!(hints),
            "{args:?}"
        );
        let event: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(event["error"], document["error"], "{args:?}");
    }
}

/// Checks the words for every missing positional and option value.
#[test]
fn test_missing_arguments_use_words_and_command_help() {
    for (args, message, path) in [
        (
            vec!["data", "upload"],
            "missing the file to upload",
            "ark data upload",
        ),
        (
            vec!["--device", "status", "data", "upload"],
            "missing the file to upload",
            "ark data upload",
        ),
        (
            vec!["-qdemulator", "data", "upload"],
            "missing the file to upload",
            "ark data upload",
        ),
        (vec!["run"], "missing the app to run", "ark run"),
        (
            vec!["data", "show"],
            "missing the slot name or id",
            "ark data show",
        ),
        (
            vec!["data", "fetch"],
            "missing the slot name or id",
            "ark data fetch",
        ),
        (
            vec!["data", "delete"],
            "missing the slot name or id",
            "ark data delete",
        ),
        (
            vec!["data", "repair"],
            "missing the slot name or id",
            "ark data repair",
        ),
        (
            vec!["completions"],
            "missing the shell for completions",
            "ark completions",
        ),
        (
            vec!["status", "--log"],
            "missing a value for --log",
            "ark status",
        ),
    ] {
        for json in [false, true] {
            let mut invocation = args.clone();
            if json {
                invocation.insert(0, "--json");
            }
            let output = ark(&invocation);
            assert_eq!(output.status.code(), Some(2), "{invocation:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains("\\n"), "{invocation:?}: {stderr}");
            let hint = format!("see `{path} --help`");
            if json {
                let document = json_output(&output);
                assert_eq!(document["error"]["message"], message, "{invocation:?}");
                assert_eq!(
                    document["error"]["hints"],
                    serde_json::json!([hint]),
                    "{invocation:?}"
                );
            } else {
                assert_eq!(
                    stderr,
                    format!("error[usage]: {message}\nhint: {hint}\n"),
                    "{invocation:?}"
                );
            }
        }
    }
}

/// Checks that usage errors print the documented text prefix on stderr and
/// nothing on stdout.
#[test]
fn documented_usage_errors_keep_the_text_prefix() {
    // Topics keep code spans in backticks in a pipe too
    let help = String::from_utf8(ark(&["help", "errors"]).stdout).unwrap();
    let prose = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(prose.contains("Exit 2 is `usage`, for invalid arguments or an unknown help topic."));
    for args in [
        vec!["bogus"],
        vec!["--timeout", "0", "status"],
        vec!["--timeout", "18446744073709551615", "status"],
        vec!["data", "show", "0"],
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

    // A missing device is the documented no-device error
    let output = ark(&["status", "--device", "hardware:palette-no-device"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(help.contains("`no-device` means no Ark is attached, or none matches --device."));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("error[no-device]: "))
    );
}

/// Checks that usage errors under `--json` print a JSON error on stdout and an
/// error event first on stderr.
#[test]
fn usage_errors_are_json_in_both_streams() {
    for args in [
        vec!["data", "fetch", "--all", "reference-genome", "--json"],
        vec!["--json", "data", "upload", "x", "--dry-run", "--unlock"],
        vec!["--json", "--timeout", "0", "status"],
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

/// Checks that --sample needs a named slot and refuses --all.
#[test]
fn test_sample_fetch_requires_a_slot_and_conflicts_with_all() {
    for (args, message) in [
        (
            vec!["data", "fetch", "--all", "--sample", "orange-ravine"],
            "--all cannot be combined with --sample",
        ),
        (
            vec!["data", "fetch", "--sample", "HG00096", "--all"],
            "--sample cannot be combined with --all",
        ),
    ] {
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr,
            format!("error[usage]: {message}\nhint: see `ark data fetch --help`\n"),
            "{args:?}"
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
    let output = ark(&["data", "fetch", "--sample", "HG00096"]);
    assert_eq!(output.status.code(), Some(2), "sample needs a slot");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error[usage]: missing the slot name or id\nhint: see `ark data fetch --help`\n",
        "missing slot guidance"
    );
}

/// Checks that firmware confirmation is listed only on its command and fails
/// as a usage error at the root, on the group and on other commands.
#[test]
fn test_firmware_confirmation_is_local_to_update() {
    // Only the update command advertises either spelling in its options
    for (path, page) in commands() {
        assert_eq!(
            page.contains("-y, --yes"),
            path == ["firmware", "update"],
            "{path:?}"
        );
    }

    // Unknown options keep the usual exit class and output in both formats
    for flag in ["-y", "--yes"] {
        for args in [
            vec![flag, "firmware", "update"],
            vec!["firmware", flag, "update"],
            vec!["firmware", "list", flag],
            vec!["status", flag],
        ] {
            for json in [false, true] {
                let mut invocation = args.clone();
                if json {
                    invocation.push("--json");
                }
                let output = ark(&invocation);
                assert_eq!(output.status.code(), Some(2), "{invocation:?}");
                if json {
                    assert_eq!(json_output(&output)["error"]["code"], "usage");
                } else {
                    assert!(output.stdout.is_empty(), "{invocation:?}");
                    assert!(
                        String::from_utf8_lossy(&output.stderr).starts_with("error[usage]: "),
                        "{invocation:?}"
                    );
                }
            }
        }
    }
}

/// The version has one JSON field and one plain line, with no stderr output.
#[test]
fn test_version_is_one_line_or_one_json_field() {
    // JSON exposes the build version alone
    let output = ark(&["--version", "--json"]);
    assert!(output.status.success());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document.as_object().unwrap().len(), 1);
    let version = document["version"].as_str().unwrap();
    semver::Version::parse(version).unwrap();
    assert!(output.stderr.is_empty());

    // Both spellings print exactly one plain line with that same version
    for flag in ["--version", "-V"] {
        let output = ark(&[flag]);
        assert!(output.status.success(), "{flag}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("ark {version}\n")
        );
        assert!(output.stderr.is_empty(), "{flag}");
    }
}

/// A checkout build marks the Cargo version as local in both output modes.
#[cfg(ark_local)]
#[test]
fn test_checkout_version_is_local() {
    let version = format!("{}-local", env!("CARGO_PKG_VERSION"));
    let output = ark(&["--version", "--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!({"version": version})
    );
    let output = ark(&["--version"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("ark {version}\n")
    );
}

/// Root and status help describe the version line and the reduced reading block.
#[test]
fn test_version_and_status_help_match_output() {
    let root = String::from_utf8(ark(&["--help"]).stdout).unwrap();
    assert!(root.contains("Show the Ark's serial, firmware, pairing and lock"));
    let version = root
        .lines()
        .find(|line| line.contains("-V, --version"))
        .unwrap();
    assert!(version.ends_with("Print the version"));

    let status = String::from_utf8(ark(&["status", "--help"]).stdout).unwrap();
    let status = status.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(status.contains("hardware (version, revision, model), firmware (version, published)"));
}

/// Doctor keeps every check and the build version in JSON, while the reading
/// report groups the checks and collapses skips after a failed connection.
#[test]
fn test_doctor_report_keeps_checks_events_and_the_final_error() {
    // A nonexistent selector prevents any connection, and CI disables lookups
    let args = ["doctor", "--device", "hardware:palette-no-device", "-v"];
    let mut invocation = args.to_vec();
    invocation.push("--json");
    let output = ark(&invocation);
    let document = json_output(&output);
    let keys: Vec<_> = document
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["version", "checks"]);
    let version = json_output(&ark(&["--version", "--json"]));
    assert_eq!(document["version"], version["version"]);

    // The flat list keeps each check, including all eight connection skips
    let checks = document["checks"].as_array().unwrap();
    let names: Vec<_> = checks
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "ark",
            "hardware",
            "emulator",
            "cache",
            "connection",
            "compatibility",
            "environment",
            "firmware",
            "sync",
            "registry",
            "pairing",
            "relay",
            "slots",
        ]
    );
    assert_eq!(checks[0]["result"], "skip");
    assert!(
        checks[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with(version["version"].as_str().unwrap())
    );
    assert_eq!(checks[4]["result"], "fail");
    assert!(checks[5..].iter().all(
        |check| check["result"] == "skip" && check["detail"] == "skipped without a connection"
    ));

    // Verbose output reports every check before the first failure ends the command
    let events: Vec<Value> = String::from_utf8(output.stderr)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for check in checks {
        let keys: Vec<_> = check
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["name", "result", "detail", "hint"]);
        let message = format!(
            "{}: {}, {}",
            check["name"].as_str().unwrap(),
            check["result"].as_str().unwrap(),
            check["detail"].as_str().unwrap()
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "step" && event["message"] == message)
                .count(),
            1,
            "{check}"
        );
    }
    let failure = checks
        .iter()
        .find(|check| check["result"] == "fail")
        .unwrap();
    let error = events
        .iter()
        .position(|event| event["event"] == "error")
        .unwrap();
    assert_eq!(events[error]["error"]["message"], failure["detail"]);
    assert_eq!(error + 1, events.len());
    if let Some(hint) = failure["hint"].as_str() {
        assert!(
            events[error]["error"]["hints"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == hint)
        );
    }

    // A pipe keeps the two headings, one collapsed row and a counted summary
    let human = ark(&args);
    assert_eq!(human.status.code(), output.status.code());
    let stdout = String::from_utf8(human.stdout).unwrap();
    assert!(stdout.starts_with("This computer\n"));
    assert!(stdout.contains("\n\nArk\n"));
    assert_eq!(stdout.matches("skipped without a connection").count(), 1);
    assert!(stdout.contains("8 checks"));
    let problems = checks
        .iter()
        .filter(|check| check["result"] == "fail")
        .count();
    let summary = format!("{problems} problem{}", if problems == 1 { "" } else { "s" });
    assert!(stdout.ends_with(&format!("\n\n{summary}\n")), "{stdout}");
    let stderr = String::from_utf8(human.stderr).unwrap();
    let error = stderr.rfind("error[").unwrap();
    assert!(stderr[..error].ends_with("\n\n"));
    assert!(stderr[error..].contains(failure["detail"].as_str().unwrap()));
    if let Some(hint) = failure["hint"].as_str() {
        assert!(stdout.contains(&format!("    hint: {hint}")));
        assert!(stderr[error..].contains(&format!("hint: {hint}")));
    }

    // With no events before it, the report opens the page and the error gets
    // the only separating newline on stderr
    let human = ark(&args[..3]);
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .starts_with("This computer\n")
    );
    assert!(
        String::from_utf8(human.stderr)
            .unwrap()
            .starts_with("\nerror[")
    );
}

/// Doctor's help and the output topic describe the report and its JSON keys.
#[test]
fn test_doctor_help_matches_the_report() {
    let output = ark(&["doctor", "--help"]);
    let help = String::from_utf8(output.stdout).unwrap();
    let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(help.contains(
        "JSON version, checks, each with name, result (ok, warn, fail or skip), detail and hint"
    ));
    assert!(!help.contains("minimum_firmware"));
    assert!(!help.contains("emulator-registry"));

    let output = ark(&["help", "output"]);
    let topic = String::from_utf8(output.stdout).unwrap();
    let topic = topic.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(topic.contains("the document holds version and checks"));
    assert!(topic.contains("the answer together in the ark check"));
    assert!(!topic.contains("update check"));
}

/// Checks that color variables cannot force escape sequences into a pipe.
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

/// Checks that help prints the same page wherever `--json` appears, and
/// unsupported flags fail as usage errors.
#[test]
fn json_selection_applies_before_help_and_usage_errors() {
    let help = ark(&["data", "fetch", "--help", "--json"]);
    assert_eq!(
        help.stdout,
        ark(&["--json", "data", "fetch", "--help"]).stdout
    );
    assert_eq!(help.stdout, ark(&["data", "fetch", "--help"]).stdout);

    // Output format flags and repeated verbosity are not part of the palette
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

/// Checks that the help pages carry their contract fields and advertise only
/// the supported commands.
#[test]
fn help_matches_the_supported_palette() {
    // Every leaf has a contract; root and group pages have none
    for (path, page) in commands() {
        let leaf = !path.is_empty() && !page.contains("Commands:");
        assert_eq!(page.contains("Contract:"), leaf, "{path:?}");
        for removed in ["Prints:", "Exit:", "see more", "see a summary"] {
            assert!(!page.contains(removed), "{path:?}: {removed}");
        }
        if !leaf {
            continue;
        }
        let mut previous = page.find("Options:").unwrap();
        for field in [
            "Contract:",
            "  Requires  ",
            "  Approval  ",
            "  Time      ",
            "  JSON      ",
            "Examples:",
        ] {
            let position = page
                .find(field)
                .unwrap_or_else(|| panic!("{path:?}: {field}"));
            assert!(position > previous, "{path:?}: {field}");
            previous = position;
        }
        for line in page.lines() {
            assert!(line.chars().count() <= 80, "{path:?}: {line}");
            assert!(!line.trim_start().starts_with("$ "), "{path:?}: {line}");
        }
    }

    // The manual advertises no unsupported command, and both help flags show
    // the recovery options
    let manual = String::from_utf8(ark(&["help", "--all"]).stdout).unwrap();
    for absent in [
        "ark app check",
        "ark run --dry-run",
        "ark run FILE --dry-run",
        "ark run app.wasm --dry-run",
        "ark cancel",
        "ark lock",
    ] {
        assert!(!manual.contains(absent), "{absent} advertised prematurely");
    }
    assert!(
        String::from_utf8(ark(&["status", "-h"]).stdout)
            .unwrap()
            .contains("--pubkey")
    );
    assert!(
        String::from_utf8(ark(&["status", "--help"]).stdout)
            .unwrap()
            .contains("--pubkey")
    );
}

/// Checks that `ark run` keeps the app's contract and examples, and that
/// `ark firmware update` takes its version as an argument, never as a flag.
#[test]
fn test_renamed_commands_keep_their_help_contracts() {
    let run = String::from_utf8(ark(&["run", "--help"]).stdout).unwrap();
    assert!(run.starts_with("Run an app, approved on your phone; print its report\n"));
    assert!(run.contains("Usage: ark run <FILE>"));
    assert!(!run.contains("--dry-run"));
    assert!(!run.contains("Commands:"));
    let contract = run.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(contract.contains(
        "JSON task, app (name, version, develop), success, paths, media, stdout or stdout_base64, stderr or stderr_base64, duration_seconds"
    ));
    assert!(run.contains("  ark run app.wasm > report.md\n  ark run app.wasm --json"));

    // Check the argument and option rows, leaving the paragraph's words free
    let update = String::from_utf8(ark(&["firmware", "update", "--help"]).stdout).unwrap();
    assert!(update.contains("Usage: ark firmware update [OPTIONS] [VERSION]"));
    let arguments = update.split_once("Arguments:\n").unwrap().1;
    assert!(arguments.contains("[VERSION]"));
    assert!(!arguments.contains("--version"));
    assert!(!arguments.contains("-V"));
    assert!(update.contains("ark firmware update 0.12.0-74d557e --yes --json"));
}

/// Checks that `ark run` opens the app before looking for an Ark.
#[test]
fn test_run_dispatches_to_app_execution() {
    // A missing app fails locally, even with a selector that matches no Ark
    let path = format!("ark-missing-app-{}.wasm", std::process::id());
    assert!(!std::path::Path::new(&path).exists());
    for json in [false, true] {
        let mut args = vec!["run", &path, "--device", "hardware:palette-no-device"];
        if json {
            args.push("--json");
        }
        let output = ark(&args);
        assert_eq!(output.status.code(), Some(1), "json={json}");
        if json {
            let document = json_output(&output);
            assert_eq!(document["error"]["code"], "file-not-found");
            assert!(
                document["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&path)
            );
        } else {
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.starts_with("error[file-not-found]: "));
            assert!(stderr.contains(&path));
        }
    }
}

/// Checks that the manual names the example apps and the emulator, and the
/// root page lists the shared options.
///
/// The example apps and the emulator are the two other corners of the loop a
/// reader arrives in.
#[test]
fn manual_carries_the_cross_references() {
    // Wrapped, so a phrase is looked for across line breaks
    let manual = String::from_utf8(ark(&["help", "--all"]).stdout).unwrap();
    let manual = manual.split_whitespace().collect::<Vec<_>>().join(" ");
    for link in [
        "https://github.com/dark-bio/examples",
        "https://github.com/dark-bio/emulator",
        "ark-emulator help agents",
    ] {
        assert!(manual.contains(link), "{link}");
    }

    // The root page lists the shared options
    let root = String::from_utf8(ark(&["--help"]).stdout).unwrap();
    assert!(root.contains("--timeout <SECONDS>"));
    assert!(root.contains("--json"));
}

/// Collects every error code the source can emit, reading the source itself.
///
/// The errors topic is then checked against what the tool does, not against a
/// second list.
fn emitted_codes() -> std::collections::BTreeSet<String> {
    /// Adds the codes found in every Rust file under `dir` to `codes`.
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
            // Constructors name the code as the string after the exit class
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
            // The Ark's reserved verdicts map to codes in match arms
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

/// Checks that the errors topic documents every error code the source emits.
#[test]
fn every_error_code_is_documented() {
    let topic = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/help/errors.md"),
    )
    .unwrap();
    for code in emitted_codes() {
        assert!(
            topic.contains(&format!("`{code}`")),
            "{code} is not in `ark help errors`"
        );
    }
}

/// Checks that the errors topic holds the rerun rules, and that the agents
/// topic names both forms of an approval.
#[test]
fn test_error_and_output_topics_keep_their_contracts() {
    let errors = String::from_utf8(ark(&["help", "errors"]).stdout).unwrap();
    for class in ["1", "2", "3", "4", "5", "6", "7", "8", "130 and 143"] {
        assert!(errors.contains(&format!("\nExit {class}\n")), "{class}");
    }

    // A class's opening names the codes that break its rerun rule
    let opening = |class: &str| {
        let (_, section) = errors.split_once(&format!("\nExit {class}\n")).unwrap();
        let (opening, _) = section.split_once("\n  - ").unwrap();
        opening.to_string()
    };
    for (class, code) in [
        ("3", "handshake-failed"),
        ("4", "environment-unknown"),
        ("4", "registry-inactive"),
    ] {
        assert!(opening(class).contains(code), "{code}");
    }

    // The output topic keeps the event fields and points failures at the errors
    // topic
    let output = String::from_utf8(ark(&["help", "output"]).stdout).unwrap();
    assert!(!output.contains("Exit 1"));
    for field in [
        "stage",
        "phase",
        "step",
        "steps",
        "percent",
        "done_bytes",
        "total_bytes",
        "remaining_seconds",
        "action",
        "where",
    ] {
        assert!(output.contains(field), "{field}");
    }
    assert!(output.contains("`ark help errors`"));

    // The agents topic names both approval forms and asks before another run
    let agents = String::from_utf8(ark(&["help", "agents"]).stdout).unwrap();
    let agents = agents.split_whitespace().collect::<Vec<_>>().join(" ");
    for text in [
        "`! Approve`",
        "`! Review`",
        "`{\"event\":\"approve\"`",
        "Ask the person before running the command again",
        "`ark help errors`",
    ] {
        assert!(agents.contains(text), "{text}");
    }
}

/// Checks that each surprising failure is named in its command's paragraph.
#[test]
fn test_command_paragraphs_name_unexpected_failures() {
    for (path, expected) in [
        ("enroll", "enrollment-required"),
        ("status", "firmware-outdated"),
        ("genuine", "registry-inactive"),
        ("run", "app-failed"),
        ("doctor", "first failed check sets the exit code"),
    ] {
        let mut args: Vec<_> = path.split_whitespace().collect();
        args.push("--help");
        let page = String::from_utf8(ark(&args).stdout).unwrap();
        let (opening, _) = page.split_once("\n\nUsage:").unwrap();
        let opening = opening.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(opening.contains(expected), "{path}");
    }
}

/// Checks that completions generate for every supported shell under the `ark`
/// name.
#[test]
fn completions_are_generated_for_the_binary_name() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = ark(&["completions", shell]);
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("ark"));
        assert!(output.stderr.is_empty());
    }
}
