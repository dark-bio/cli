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
            public: true,
            wording: "Sample collection".into(),
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
            wording: "Your sample group".into(),
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
            state: SlotState::Filled as i32,
            origin: schema::SlotOrigin::Personal as i32,
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
            state: SlotState::Empty as i32,
            origin: schema::SlotOrigin::Reference as i32,
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

/// Builds the six slots in the order the Ark lists them, all filled, with long
/// invented texts on the two new personal slots.
fn genome_slots() -> Vec<SlotStatus> {
    use schema::{SlotKind, SlotOrigin};

    // Sizes and builds that exercise the table's column widths
    let mut slots: Vec<_> = [
        (SlotKind::ReferenceGenome, "GRCh38.p14", "", 1_022_427_344),
        (SlotKind::GeneAnnotations, "GRCh38.p14", "", 215_062_938),
        (SlotKind::SmallVariants, "GRCh38.p13", "", 307_547_341),
        (SlotKind::VariantCatalog, "GRCh38.p14", "157", 6_227_702_579),
    ]
    .into_iter()
    .map(|(kind, build, version, bytes)| SlotStatus {
        kind: kind as i32,
        state: SlotState::Filled as i32,
        origin: if kind == SlotKind::SmallVariants {
            SlotOrigin::Personal as i32
        } else {
            SlotOrigin::Reference as i32
        },
        deps: if kind == SlotKind::SmallVariants {
            vec![]
        } else {
            vec![SlotKind::SmallVariants as i32]
        },
        build: build.into(),
        version: version.into(),
        bytes,
        ..Default::default()
    })
    .collect();

    // The new personal slots carry texts as long as the Ark's, which their
    // pages wrap
    slots.splice(
        3..3,
        [
            SlotStatus {
                kind: SlotKind::StructuralVariants as i32,
                name: "Structural Variants".into(),
                desc: "A third sample collection of plain values with grouped labels. Each \
                group names a few values that belong together, and a value may sit in \
                more than one group."
                    .into(),
                format: "Plain text with one group on each line, its label first and its \
                values after it, separated by spaces. Labels are lowercase words, and \
                values are whole numbers. A group may repeat an earlier value, and a line \
                may end with a comment after a hash sign. Blank lines are skipped. The \
                collection stays under one thousand lines, so its page wraps this text \
                across several lines on a narrow screen."
                    .into(),
                state: SlotState::Filled as i32,
                origin: SlotOrigin::Personal as i32,
                deps: vec![SlotKind::SmallVariants as i32],
                build: "GRCh38.p13".into(),
                bytes: 2_097_152,
                ..Default::default()
            },
            SlotStatus {
                kind: SlotKind::CopyNumberVariants as i32,
                name: "Copy Number Variants".into(),
                desc: "A fourth sample collection of plain values, each with a count of how \
                often it appears."
                    .into(),
                format: "Plain text with one value and its count on each line, separated by a \
                single space. Counts are whole numbers from zero to nine, and values keep \
                the order they were first seen in. A line may end with a comment after a \
                hash sign, and blank lines are skipped. The collection stays under one \
                hundred lines, so its page wraps this text across several lines on a \
                narrow screen."
                    .into(),
                state: SlotState::Filled as i32,
                origin: SlotOrigin::Personal as i32,
                deps: vec![SlotKind::SmallVariants as i32],
                build: "GRCh38.p13".into(),
                bytes: 1_048_576,
                ..Default::default()
            },
        ],
    );
    slots
}

/// Runs one scenario in a child test process, capturing its real stdout and
/// stderr so results and hints stay distinguishable.
fn capture(scenario: &str, json: bool) -> (String, String) {
    // Run this test again as a child with UTF-8 marks and no color
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data::scenarios::inventory_output",
            "--nocapture",
        ])
        .env("ARK_TEST_DATA_SCENARIO", scenario)
        .env("ARK_TEST_DATA_JSON", json.to_string())
        .env("NO_COLOR", "1")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    // Keep only the result between the markers, past the test harness's lines
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

/// Checks that path maps and slots print exact JSON, a readable view and the
/// right hints.
///
/// Full, partial and empty path maps are covered, with availability marked once
/// per missing subtree.
#[test]
fn inventory_output() {
    // As the child, print one scenario between result markers
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
            "six-slots" => listing(&output, &genome_slots()).unwrap(),
            "show5" => show(&output, &genome_slots(), 5).unwrap(),
            "show6" => show(&output, &genome_slots(), 6).unwrap(),
            "missing-small-variants" | "single-dependent" => {
                let mut slots = genome_slots();
                for slot in &mut slots {
                    slot.state = SlotState::Empty as i32;
                }
                if scenario == "single-dependent" {
                    slots.retain(|slot| [3, 5].contains(&slot.kind));
                }
                listing(&output, &slots).unwrap();
            }
            "show41" => show(&output, &slots(), 41).unwrap(),
            "show42" => show(&output, &slots(), 42).unwrap(),
            _ => panic!("unexpected scenario {scenario}"),
        }
        writeln!(std::io::stdout(), "</result>").unwrap();
        return;
    }

    // Every path map prints the same entries as JSON or a tree, hinting at the
    // slot list when some are unavailable
    let expected = json!({"paths":[
        {
            "path":"v1/sample", "directory":true, "grantable":true, "available":true,
            "description":"A sample collection.", "format":"", "examples":[],
            "public":true, "wording":"Sample collection",
        },
        {
            "path":"v1/sample/label", "directory":false, "grantable":false, "available":true,
            "description":"A sample label.", "format":"Plain text.", "examples":["first","second"],
            "public":false, "wording":"",
        },
        {
            "path":"v1/sample/group", "directory":true, "grantable":true, "available":false,
            "description":"A sample group.", "format":"", "examples":[],
            "public":false, "wording":"Your sample group",
        },
        {
            "path":"v1/sample/group/<item>", "directory":true, "grantable":false, "available":false,
            "description":"A sample item.", "format":"", "examples":["one"],
            "public":false, "wording":"",
        },
        {
            "path":"v1/sample/group/<item>/value", "directory":false, "grantable":false, "available":false,
            "description":"A sample value.", "format":"Decimal text.", "examples":["7","9"],
            "public":false, "wording":"",
        },
        {
            "path":"v1/other", "directory":true, "grantable":false, "available":false,
            "description":"Another sample collection.", "format":"", "examples":[],
            "public":false, "wording":"",
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
                        "  / directory, + grantable, ! unavailable; details with --json\n  v1/sample/ +\n    label       first, second\n    group/ + !\n      <item>/   one\n        value   7, 9\n  v1/other/ !\n"
                    } else {
                        "  / directory, + grantable, ! unavailable; details with --json\n  v1/sample/ +\n    label       first, second\n    group/ +\n      <item>/   one\n        value   7, 9\n  v1/other/\n"
                    }
                );
                if unavailable {
                    assert_eq!(
                        stderr,
                        "\nhint: some paths are unavailable; run `ark data list`\n"
                    );
                }
            } else {
                assert_eq!(stdout, "No dataset paths\n", "empty path result");
            }
            if !unavailable {
                assert!(stderr.is_empty());
            }
            assert!(!stdout.contains('\x1b'));
        }
    }

    // The slot list prints both slots and hints at fetching the missing
    // reference
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
        "SLOT  STATE     BUILD     VERSION     SIZE\n41    \u{2713} filled  sample-a  1        2.0 KiB\n42    \u{00b7} empty   -         -            0 B\n"
    );
    assert_eq!(stderr, "\nhint: run `ark data fetch 42`\n");

    // Each slot's page shows every field, wrapping long texts under their label
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
            assert!(joined.ends_with("State \u{2713} filled Origin personal Damage - Dependencies none Size 2.0 KiB Build sample-a Version 1 Download - Required by 42 Cached no"));
        } else {
            assert!(joined.starts_with("Slot 42 Id 42 Name Sample B Description "));
            assert!(joined.contains("State \u{00b7} empty Origin reference Damage - Dependencies 41 (filled) Size 0 B Build - Version - Download url https://example.test/sample Download size 4.0 KiB Download sha256 "));
            assert!(joined.ends_with("Required by none Cached no"));
        }
    }
}

/// Six typical slots print as six rows of five columns within 80 columns.
#[test]
fn test_data_list_fits_six_slots_at_eighty_columns() {
    let (stdout, stderr) = capture("six-slots", false);
    assert!(stderr.is_empty());
    assert_eq!(stdout.lines().count(), 7);
    assert!(
        stdout
            .lines()
            .all(|line| console::measure_text_width(line) <= 80)
    );
    assert_eq!(
        stdout,
        concat!(
            "SLOT                  STATE     BUILD       VERSION       SIZE\n",
            "reference-genome      ✓ filled  GRCh38.p14  -        975.1 MiB\n",
            "gene-annotations      ✓ filled  GRCh38.p14  -        205.1 MiB\n",
            "small-variants        ✓ filled  GRCh38.p13  -        293.3 MiB\n",
            "structural-variants   ✓ filled  GRCh38.p13  -          2.0 MiB\n",
            "copy-number-variants  ✓ filled  GRCh38.p13  -          1.0 MiB\n",
            "variant-catalog       ✓ filled  GRCh38.p14  157        5.8 GiB\n",
        )
    );
}

/// Checks that an unfilled slot gets one hint naming every slot that requires
/// it, in list order, and that a lone dependent reads in the singular.
#[test]
fn test_data_list_groups_dependency_hints() {
    for (scenario, message) in [
        (
            "missing-small-variants",
            "reference-genome, gene-annotations, structural-variants, copy-number-variants and variant-catalog require small-variants; fill it first",
        ),
        (
            "single-dependent",
            "structural-variants requires small-variants; fill it first",
        ),
    ] {
        for json in [false, true] {
            let (_, stderr) = capture(scenario, json);
            if json {
                assert_eq!(
                    serde_json::from_str::<Value>(stderr.trim()).unwrap(),
                    json!({"event": "hint", "message": message}),
                    "{scenario}"
                );
            } else {
                assert_eq!(
                    stderr.split_whitespace().collect::<Vec<_>>().join(" "),
                    format!("hint: {message}"),
                    "{scenario}"
                );
                assert!(
                    stderr
                        .lines()
                        .all(|line| console::measure_text_width(line) <= 80)
                );
            }
        }
    }
}

/// Checks that the new personal slots' pages keep their texts whole and wrap
/// them within 80 columns.
#[test]
fn test_personal_slot_descriptions_fit_eighty_columns() {
    for (scenario, name) in [
        ("show5", "structural-variants"),
        ("show6", "copy-number-variants"),
    ] {
        let (stdout, stderr) = capture(scenario, true);
        assert!(stderr.is_empty());
        let slot: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(slot["slot"], name, "{scenario}");
        assert_eq!(slot["origin"], "personal", "{scenario}");
        assert_eq!(slot["requires"], json!(["small-variants"]), "{scenario}");

        // The reading view holds the same description and format as the JSON
        let (stdout, stderr) = capture(scenario, false);
        assert!(stderr.is_empty());
        assert!(
            stdout
                .lines()
                .all(|line| console::measure_text_width(line) <= 80)
        );
        let joined = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(joined.contains(slot["description"].as_str().unwrap()));
        assert!(joined.contains(slot["format"].as_str().unwrap()));
        assert!(joined.contains("Dependencies small-variants (filled)"));
    }
}

/// Uploads a structural variants file to a scripted Ark whose small-variants
/// slot is in `state`, checking which requests reach it.
fn upload_with_requirement(
    output: Output,
    clock: &darkbio_clock::TestClock,
    state: SlotState,
    dry_run: bool,
) -> Result<(), Error> {
    use darkbio_connect::{
        Ark, TrustMode,
        trust::{Environment, Realm},
    };
    use schema::host_to_ark::Content;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Record each request, so the order of identification, listing and upload
    // can be checked
    let bytes = b"invented upload fixture\n";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut peer = crate::testing::peer::Peer::spawn(
        &clock.clock(),
        Box::new({
            let requests = requests.clone();
            move |session, request, responder| {
                let deadline = session.clock().now() + Duration::from_secs(10);
                match request {
                    Content::DeviceInfo(_) => {
                        responder
                            .reply(
                                schema::DeviceInfoResponse {
                                    cloud_synced: true,
                                    cloud_clock: session
                                        .clock()
                                        .system_time()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs(),
                                    ..Default::default()
                                },
                                deadline,
                            )
                            .unwrap();
                    }
                    Content::SlotIdentify(request) => {
                        requests.lock().unwrap().push("identify");
                        assert_eq!(request.chunk, bytes);
                        responder
                            .reply(
                                schema::SlotIdentifyResponse {
                                    kind: schema::SlotKind::StructuralVariants as i32,
                                    conf: schema::SlotConfidence::High as i32,
                                    ..Default::default()
                                },
                                deadline,
                            )
                            .unwrap();
                    }
                    Content::SlotList(_) => {
                        requests.lock().unwrap().push("list");
                        let mut slots = genome_slots();
                        for slot in &mut slots {
                            if slot.kind == 3 {
                                slot.state = state as i32;
                            } else if slot.kind == 5 {
                                slot.state = SlotState::Empty as i32;
                            }
                        }
                        responder
                            .reply(schema::SlotListResponse { slots }, deadline)
                            .unwrap();
                    }
                    Content::SlotUploadStart(request) => {
                        requests.lock().unwrap().push("start");
                        assert_eq!(state, SlotState::Filled);
                        assert!(!dry_run);
                        assert_eq!(request.kind, 5);
                        assert_eq!(request.chunk, bytes);
                        responder
                            .reply(schema::SlotUploadStartResponse { session: 7 }, deadline)
                            .unwrap();
                    }
                    Content::SlotUploadProcess(request) => {
                        requests.lock().unwrap().push("process");
                        assert_eq!(request.session, 7);
                        responder
                            .reply(
                                schema::SlotUploadProcessResponse {
                                    phase_in: 1,
                                    phase_progress: 10_000,
                                    phases: vec![schema::SlotPhase {
                                        name: "Indexing".into(),
                                        desc: String::new(),
                                    }],
                                    ..Default::default()
                                },
                                deadline,
                            )
                            .unwrap();
                    }
                    _ => panic!("unexpected upload request: {request:?}"),
                }
                true
            }
        }),
    );

    // Run the upload over the scripted connection
    let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
    let (ark, _) = Ark::attach(peer.stream(), &trust, |_| {
        Some((Environment::Develop, Realm::Hardware))
    })
    .unwrap();
    let context = Context {
        options: Cli::parse_from(["ark"]).options,
        interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
        output,
    };
    let result = upload_file(
        &context,
        &ark.client(),
        "sample.sv.vcf",
        bytes.len() as u64,
        &mut std::io::Cursor::new(bytes),
        None,
        dry_run,
    );
    let expected = if state == SlotState::Filled && !dry_run {
        vec!["identify", "list", "start", "process"]
    } else {
        vec!["identify", "list"]
    };
    assert_eq!(*requests.lock().unwrap(), expected);
    result
}

/// Checks that an unfilled required slot, empty or damaged, fails the upload
/// before it starts or asks for an approval.
#[test]
fn test_upload_refuses_unfilled_requirement_before_approval() {
    for captured in crate::testing::narration(
        "data::scenarios::test_upload_refuses_unfilled_requirement_before_approval",
        |output, clock| {
            for state in [SlotState::Empty, SlotState::Damaged] {
                let error =
                    upload_with_requirement(output.clone(), clock, state, false).unwrap_err();
                assert_eq!(error.code, "dependency-missing", "{state:?}");
                assert_eq!(error.class, 5, "{state:?}");
                assert_eq!(
                    error.message,
                    "structural-variants requires small-variants, which is not filled",
                    "{state:?}"
                );
                assert_eq!(error.hints, ["fill small-variants first"], "{state:?}");
                output.error(&error);
            }
        },
    ) {
        captured.approvals(&[]);
        assert!(!captured.stderr.contains("Approve"));
        assert!(captured.stdout.is_empty());
        if captured.json {
            let events = captured.events();
            let errors: Vec<_> = events
                .iter()
                .filter(|event| event["event"] == "error")
                .collect();
            assert_eq!(errors.len(), 2);
            for error in errors {
                assert_eq!(
                    error["error"],
                    json!({
                        "code": "dependency-missing",
                        "message": "structural-variants requires small-variants, which is not filled",
                        "hints": ["fill small-variants first"],
                    })
                );
            }
        } else {
            let text = captured
                .stderr
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(text.contains("error[dependency-missing]: structural-variants requires small-variants, which is not filled"));
            assert!(text.contains("hint: fill small-variants first"));
        }
    }
}

/// Checks that a filled required slot lets the upload start, ask for its
/// approval and finish.
#[test]
fn test_upload_accepts_filled_requirement() {
    for captured in crate::testing::narration(
        "data::scenarios::test_upload_accepts_filled_requirement",
        |output, clock| {
            upload_with_requirement(output, clock, SlotState::Filled, false).unwrap();
        },
    ) {
        captured.approvals(&[("upload", Some("phone"))]);
        captured.contains(&[
            "Identified as structural-variants",
            "Approve the upload on your phone",
        ]);
        if captured.json {
            let value: Value = serde_json::from_str(&captured.stdout).unwrap();
            assert_eq!(value["slot"], "structural-variants");
            assert_eq!(value["uploaded_bytes"], 24);
            assert_eq!(value["phases"], json!(["Indexing"]));
        }
    }
}

/// Checks that a dry run reports an empty required slot instead of failing,
/// and starts nothing.
#[test]
fn test_upload_dry_run_reports_empty_requirement() {
    for captured in crate::testing::narration(
        "data::scenarios::test_upload_dry_run_reports_empty_requirement",
        |output, clock| {
            upload_with_requirement(output, clock, SlotState::Empty, true).unwrap();
        },
    ) {
        captured.approvals(&[]);
        assert!(!captured.stderr.contains("Approve"));
        if captured.json {
            let value: Value = serde_json::from_str(&captured.stdout).unwrap();
            assert_eq!(value["state"], "empty");
            assert_eq!(value["requires"], json!(["small-variants"]));
            assert_eq!(value["uploaded_bytes"], 0);
        } else {
            assert!(captured.stdout.contains("small-variants (not filled)"));
        }
    }
}
