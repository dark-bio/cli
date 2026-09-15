// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Fixture scenarios through the command output writers, without an attached Ark.

use super::*;
use crate::{args::Cli, output::Output};
use clap::Parser;
use std::io::Write;

/// Builds an invented path map with available entries and missing subtrees.
fn paths() -> Vec<schema::DatasetPath> {
    vec![
        schema::DatasetPath {
            path: "v1/sample".into(),
            directory: true,
            grantable: true,
            available: true,
            desc: "A sample collection.".into(),
            ..Default::default()
        },
        schema::DatasetPath {
            path: "v1/sample/label".into(),
            available: true,
            desc: "A sample label.".into(),
            format: "Plain text.".into(),
            examples: vec!["first".into(), "second".into()],
            ..Default::default()
        },
        schema::DatasetPath {
            path: "v1/sample/group".into(),
            directory: true,
            grantable: true,
            desc: "A sample group.".into(),
            ..Default::default()
        },
        schema::DatasetPath {
            path: "v1/sample/group/<item>".into(),
            directory: true,
            desc: "A sample item.".into(),
            examples: vec!["one".into()],
            ..Default::default()
        },
        schema::DatasetPath {
            path: "v1/sample/group/<item>/value".into(),
            desc: "A sample value.".into(),
            format: "Decimal text.".into(),
            examples: vec!["7".into(), "9".into()],
            ..Default::default()
        },
        schema::DatasetPath {
            path: "v1/other".into(),
            directory: true,
            desc: "Another sample collection.".into(),
            ..Default::default()
        },
    ]
}

/// Builds two invented slots with descriptions and formats that need wrapping.
fn slots() -> Vec<SlotStatus> {
    vec![
        SlotStatus {
            kind: 41,
            name: "Sample A".into(),
            desc: "A sample collection of plain values with short labels. Each label identifies \
                one value in this small collection of example records."
                .into(),
            format: "Plain text with one label and one value on each line. Separate the label \
                and value with a space, then end the line with a newline."
                .into(),
            state: SlotState::StateFilled as i32,
            origin: schema::SlotOrigin::OriginPersonal as i32,
            bytes: 2048,
            build: "sample-a".into(),
            version: "1".into(),
            ..Default::default()
        },
        SlotStatus {
            kind: 42,
            name: "Sample B".into(),
            desc: "Another sample collection of plain values with simple labels. The labels \
                keep related example records together in this small collection."
                .into(),
            format: "Plain text with one value on each line. Keep the lines in label order \
                and finish every line with a newline, including the last line."
                .into(),
            state: SlotState::StateEmpty as i32,
            origin: schema::SlotOrigin::OriginReference as i32,
            deps: vec![41],
            download: Some(schema::SlotDownload {
                url: "https://example.test/sample".into(),
                bytes: 4096,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            }),
            ..Default::default()
        },
    ]
}

/// Runs one scenario in a child test process, capturing its real stdout and
/// stderr so results and hints can be told apart.
fn capture(scenario: &str, json: bool) -> (String, String) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data::scenarios::inventory_output",
            "--nocapture",
        ])
        .env("ARK_TEST_DATA_SCENARIO", scenario)
        .env("ARK_TEST_DATA_JSON", json.to_string())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stdout = stdout
        .split_once("<result>\n")
        .unwrap()
        .1
        .split_once("</result>\n")
        .unwrap()
        .0
        .to_string();
    (stdout, String::from_utf8(output.stderr).unwrap())
}

/// Full, partial and empty path maps and every slot print exact JSON, a readable
/// view and the right hints, with availability marked once per missing subtree.
#[test]
fn inventory_output() {
    if let Ok(scenario) = std::env::var("ARK_TEST_DATA_SCENARIO") {
        let mut options = Cli::parse_from(["ark"]).options;
        options.json = std::env::var("ARK_TEST_DATA_JSON").unwrap() == "true";
        options.quiet = scenario == "quiet";
        let output = Output::new(&options);
        writeln!(std::io::stdout(), "<result>").unwrap();
        match scenario.as_str() {
            "mixed" | "quiet" => super::paths::print(&output, &paths()).unwrap(),
            "loaded" => {
                let mut paths = paths();
                for path in &mut paths {
                    path.available = true;
                }
                super::paths::print(&output, &paths).unwrap();
            }
            "empty" => super::paths::print(&output, &[]).unwrap(),
            "list" => listing(&output, &slots()).unwrap(),
            "show41" => show(&output, &slots(), 41).unwrap(),
            "show42" => show(&output, &slots(), 42).unwrap(),
            _ => panic!("unexpected scenario {scenario}"),
        }
        writeln!(std::io::stdout(), "</result>").unwrap();
        return;
    }
    let expected = json!({"paths":[
        {
            "path":"v1/sample", "directory":true, "grantable":true, "available":true,
            "description":"A sample collection.", "format":"", "examples":[],
        },
        {
            "path":"v1/sample/label", "directory":false, "grantable":false, "available":true,
            "description":"A sample label.", "format":"Plain text.", "examples":["first","second"],
        },
        {
            "path":"v1/sample/group", "directory":true, "grantable":true, "available":false,
            "description":"A sample group.", "format":"", "examples":[],
        },
        {
            "path":"v1/sample/group/<item>", "directory":true, "grantable":false, "available":false,
            "description":"A sample item.", "format":"", "examples":["one"],
        },
        {
            "path":"v1/sample/group/<item>/value", "directory":false, "grantable":false, "available":false,
            "description":"A sample value.", "format":"Decimal text.", "examples":["7","9"],
        },
        {
            "path":"v1/other", "directory":true, "grantable":false, "available":false,
            "description":"Another sample collection.", "format":"", "examples":[],
        },
    ]});
    for json in [false, true] {
        for scenario in ["mixed", "quiet", "loaded", "empty"] {
            let (stdout, stderr) = capture(scenario, json);
            let unavailable = matches!(scenario, "mixed" | "quiet");
            if json {
                let mut expected = expected.clone();
                if scenario == "loaded" {
                    for entry in expected["paths"].as_array_mut().unwrap() {
                        entry["available"] = true.into();
                    }
                } else if scenario == "empty" {
                    expected["paths"] = json!([]);
                }
                assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), expected);
                if unavailable {
                    assert_eq!(
                        serde_json::from_str::<Value>(stderr.trim()).unwrap(),
                        json!({"event":"hint","message":"some paths are unavailable; run `ark data list`"})
                    );
                }
            } else if scenario != "empty" {
                assert_eq!(
                    stdout,
                    if unavailable {
                        "  / directory, + grantable, ! unavailable; details with --json\n  v1/sample/ +\n    label\n    group/ + !\n      <item>/\n        value\n  v1/other/ !\n"
                    } else {
                        "  / directory, + grantable, ! unavailable; details with --json\n  v1/sample/ +\n    label\n    group/ +\n      <item>/\n        value\n  v1/other/\n"
                    }
                );
                if unavailable {
                    assert_eq!(
                        stderr,
                        "hint: some paths are unavailable; run `ark data list`\n"
                    );
                }
            } else {
                assert_eq!(stdout, "  No dataset paths\n");
            }
            if !unavailable {
                assert!(stderr.is_empty());
            }
            assert!(!stdout.contains('\x1b'));
        }
    }
    let expected = json!({"slots":[
        {
            "slot":"41", "id":41, "name":"Sample A",
            "description":"A sample collection of plain values with short labels. Each label identifies one value in this small collection of example records.",
            "format":"Plain text with one label and one value on each line. Separate the label and value with a space, then end the line with a newline.",
            "state":"filled", "origin":"personal", "damage":null, "requires":[],
            "size_bytes":2048, "build":"sample-a", "version":"1", "download":null,
        },
        {
            "slot":"42", "id":42, "name":"Sample B",
            "description":"Another sample collection of plain values with simple labels. The labels keep related example records together in this small collection.",
            "format":"Plain text with one value on each line. Keep the lines in label order and finish every line with a newline, including the last line.",
            "state":"empty", "origin":"reference", "damage":null, "requires":["41"],
            "size_bytes":0, "build":null, "version":null,
            "download":{
                "url":"https://example.test/sample", "size_bytes":4096,
                "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            },
        },
    ]});
    let (stdout, stderr) = capture("list", true);
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), expected);
    assert_eq!(
        serde_json::from_str::<Value>(stderr.trim()).unwrap(),
        json!({"event":"hint","message":"run `ark data fetch 42`"})
    );
    let (stdout, stderr) = capture("list", false);
    assert_eq!(
        stdout,
        "  SLOT  STATE      ORIGIN     BUILD     VERSION     SIZE  REQUIRES\n  41    ok filled  personal   sample-a  1        2.0 KiB  none\n  42    - empty    reference  -         -            0 B  41 (filled)\n"
    );
    assert_eq!(stderr, "hint: run `ark data fetch 42`\n");
    for (index, id) in [41, 42].into_iter().enumerate() {
        let scenario = format!("show{id}");
        let (stdout, stderr) = capture(&scenario, true);
        assert!(stderr.is_empty());
        let mut slot = expected["slots"][index].clone();
        slot["required_by"] = if id == 41 { json!(["42"]) } else { json!([]) };
        slot["cached"] = false.into();
        assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), slot);
        let (stdout, stderr) = capture(&scenario, false);
        assert!(stderr.is_empty());
        assert!(
            stdout
                .lines()
                .all(|line| console::measure_text_width(line) <= 80)
        );
        let joined = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(joined.contains(slot["description"].as_str().unwrap()));
        assert!(joined.contains(slot["format"].as_str().unwrap()));
        for label in ["Description", "Format"] {
            let mut lines = stdout
                .lines()
                .skip_while(|line| !line.trim_start().starts_with(label));
            let first = lines.next().unwrap();
            let indent = if id == 41 { 16 } else { 19 };
            assert!(lines.next().unwrap().starts_with(&" ".repeat(indent)));
            assert!(first.len() > indent);
        }
        if id == 41 {
            assert!(joined.starts_with("Slot 41 Id 41 Name Sample A Description "));
            assert!(joined.ends_with("State ok filled Origin personal Damage - Dependencies none Size 2.0 KiB Build sample-a Version 1 Download - Required by 42 Cached no"));
        } else {
            assert!(joined.starts_with("Slot 42 Id 42 Name Sample B Description "));
            assert!(joined.contains("State - empty Origin reference Damage - Dependencies 41 (filled) Size 0 B Build - Version - Download url https://example.test/sample Download size 4.0 KiB Download sha256 "));
            assert!(joined.ends_with("Required by none Cached no"));
        }
    }
}
