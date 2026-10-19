// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Tests of a sample filling the three personal slots, against a scripted Ark.

use super::*;
use crate::testing::peer::Peer;
use darkbio_connect::{
    Ark, TrustMode, schema,
    trust::{Environment, Realm},
};
use schema::host_to_ark::Content;
use std::sync::{Arc, Mutex};

/// HG00096's three files as hashed on 14 October 2026, apart from the pin
/// tables.
const FILES: [(i32, &str, &str, u64, &str); 3] = [
    (
        3,
        "small-variants",
        "hard-filtered.vcf.gz",
        437_027_190,
        "488f8b0fc05cfae2a2c7b4208367abefa8401fcef3151e32feb5587d71303260",
    ),
    (
        5,
        "structural-variants",
        "sv.vcf.gz",
        17_896_888,
        "06f7c1956c89af37e1e8cb808c1036606be0908e8b07f41a012bce60ff7ebdd9",
    ),
    (
        6,
        "copy-number-variants",
        "cnv.vcf.gz",
        59_421,
        "de5959c6dda6b78a997589a39d666083681725610ee1400a286a0441cd2466af",
    ),
];

/// Builds an inventory that lists the slots in another order than a sample
/// fills them.
fn inventory() -> Vec<SlotStatus> {
    [6, 1, 5, 3, 4]
        .into_iter()
        .map(|kind| SlotStatus {
            kind,
            origin: if [3, 5, 6].contains(&kind) {
                SlotOrigin::Personal
            } else {
                SlotOrigin::Reference
            } as i32,
            state: if [3, 5, 6].contains(&kind) {
                SlotState::Empty
            } else {
                SlotState::Filled
            } as i32,
            deps: if kind == 3 { vec![] } else { vec![3] },
            ..Default::default()
        })
        .collect()
}

/// Attaches a scripted Ark that fails the test on any request, for plans that
/// must stay local.
fn idle_ark(clock: &TestClock) -> (Peer, Ark) {
    let mut peer = Peer::spawn(
        &clock.clock(),
        Box::new(|_, request, _| panic!("planning sent a request: {request:?}")),
    );
    let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
    let (ark, _) = Ark::attach(peer.stream(), &trust, |_| {
        Some((Environment::Develop, Realm::Hardware))
    })
    .unwrap();
    (peer, ark)
}

/// Checks that a dry run plans the three files with their measured pins, and
/// transfers nothing.
#[test]
fn test_sample_dry_run_plans_three_files() {
    for captured in crate::testing::narration(
        "data::reference::tests::samples::test_sample_dry_run_plans_three_files",
        |output, clock| {
            let (_peer, ark) = idle_ark(clock);
            let directory = Directory::new();
            let context = Context {
                options: crate::args::Cli::parse_from(["ark"]).options,
                interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                output,
            };
            fetch(
                &context,
                &ark.client(),
                &inventory(),
                Target::Sample(sample("notable-owen")),
                true,
                Some(&directory.0),
                false,
            )
            .unwrap();
            assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
        },
    ) {
        assert!(captured.stderr.is_empty());
        if captured.json {
            let document: Value = serde_json::from_str(&captured.stdout).unwrap();
            let expected: Vec<_> = FILES.iter().map(|(id, slot, suffix, size, digest)| json!({
                "slot": slot, "id": id,
                "url": format!("https://1000genomes-dragen-v4-4-7.s3.amazonaws.com/data/individuals/hg38-alt_masked.cnv.graph.hla.methyl_cg.rna-11-r5.0-2/HG00096/HG00096.{suffix}"),
                "size_bytes": size, "sha256": digest, "cached": false,
                "outcome": "planned", "error": null, "sample": "HG00096", "name": "notable-owen",
            })).collect();
            assert_eq!(document, json!({"fetched": expected}));
        } else {
            assert_eq!(
                captured
                    .stdout
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                ["SLOT", "SIZE", "CACHED", "OUTCOME"]
            );
            let rows: Vec<_> = captured
                .stdout
                .lines()
                .skip(1)
                .map(|row| row.split_whitespace().next().unwrap())
                .collect();
            assert_eq!(
                rows,
                [
                    "small-variants",
                    "structural-variants",
                    "copy-number-variants"
                ]
            );
        }
    }
}

/// Checks that a fetch fails before any transfer when the Ark lacks one of
/// the three slots.
#[test]
fn test_sample_requires_all_three_slots() {
    for captured in crate::testing::narration(
        "data::reference::tests::samples::test_sample_requires_all_three_slots",
        |output, clock| {
            let (_peer, ark) = idle_ark(clock);
            let context = Context {
                options: crate::args::Cli::parse_from(["ark"]).options,
                interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                output,
            };
            for (id, name) in [
                (3, "small-variants"),
                (5, "structural-variants"),
                (6, "copy-number-variants"),
            ] {
                let slots: Vec<_> = inventory()
                    .into_iter()
                    .filter(|slot| slot.kind != id)
                    .collect();
                for dry_run in [false, true] {
                    let error = fetch(
                        &context,
                        &ark.client(),
                        &slots,
                        Target::Sample(sample("HG00096")),
                        dry_run,
                        None,
                        true,
                    )
                    .unwrap_err();
                    assert_eq!((error.class, error.code), (1, "invalid-slot"), "{id}");
                    assert_eq!(error.message, format!("the Ark has no slot {name}"), "{id}");
                }
            }
        },
    ) {
        assert!(captured.stdout.is_empty());
        assert!(captured.stderr.is_empty());
    }
}

/// Checks that any personal slot holding data refuses the sample, unknown
/// kinds included, and that the slots appear in list order.
#[test]
fn test_sample_refuses_occupied_personal_slots() {
    for captured in crate::testing::narration(
        "data::reference::tests::samples::test_sample_refuses_occupied_personal_slots",
        |output, clock| {
            let (_peer, ark) = idle_ark(clock);
            let directory = Directory::new();
            let context = Context {
                options: crate::args::Cli::parse_from(["ark"]).options,
                interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                output,
            };

            // One occupied slot reads in the singular, and its hint repairs a damaged
            // slot or deletes a filled one
            for (id, name) in [
                (3, "small-variants"),
                (5, "structural-variants"),
                (6, "copy-number-variants"),
                (42, "42"),
            ] {
                for (state, action) in [
                    (SlotState::Filled as i32, "delete"),
                    (SlotState::Damaged as i32, "repair"),
                    (99, "delete"),
                ] {
                    let mut slots = inventory();
                    slots.push(SlotStatus {
                        kind: 42,
                        origin: SlotOrigin::Personal as i32,
                        state: SlotState::Empty as i32,
                        ..Default::default()
                    });
                    slots.iter_mut().find(|slot| slot.kind == id).unwrap().state = state;
                    for dry_run in [false, true] {
                        let error = fetch(
                            &context,
                            &ark.client(),
                            &slots,
                            Target::Sample(sample("HG00096")),
                            dry_run,
                            Some(&directory.0),
                            false,
                        )
                        .unwrap_err();
                        assert_eq!(
                            (error.class, error.code),
                            (5, "slot-filled"),
                            "{id}/{state}"
                        );
                        assert_eq!(
                            error.message,
                            format!(
                                "{name} holds data, and a sample fills only empty personal slots"
                            ),
                            "{id}/{state}"
                        );
                        assert_eq!(
                            error.hints,
                            [format!("run `ark data {action} {name}`")],
                            "{id}/{state}"
                        );
                    }
                }
            }

            // Several occupied slots keep the Ark's list order, in the message and the
            // hints
            for count in [2, 3] {
                let mut slots = inventory();
                slots[0].state = SlotState::Filled as i32;
                slots[2].state = SlotState::Damaged as i32;
                if count == 3 {
                    slots[3].state = SlotState::Filled as i32;
                }
                for dry_run in [false, true] {
                    let error = fetch(
                        &context,
                        &ark.client(),
                        &slots,
                        Target::Sample(sample("HG00096")),
                        dry_run,
                        Some(&directory.0),
                        false,
                    )
                    .unwrap_err();
                    assert_eq!((error.class, error.code), (5, "slot-filled"));
                    assert_eq!(
                        error.message,
                        if count == 2 {
                            "copy-number-variants and structural-variants hold data, and a sample fills only empty personal slots"
                        } else {
                            "copy-number-variants, structural-variants and small-variants hold data, and a sample fills only empty personal slots"
                        }
                    );
                    let mut hints = vec![
                        "run `ark data delete copy-number-variants`",
                        "run `ark data repair structural-variants`",
                    ];
                    if count == 3 {
                        hints.push("run `ark data delete small-variants`");
                    }
                    assert_eq!(error.hints, hints);
                }
            }
            assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
        },
    ) {
        assert!(captured.stdout.is_empty());
        assert!(captured.stderr.is_empty());
    }
}

/// How the structural variant file's transfer goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// Every file succeeds.
    None,
    /// The downloaded bytes have the wrong digest.
    Digest,
    /// The downloaded file is longer than its pin.
    Size,
    /// The upload's approval is refused.
    Approval,
}

/// Fetches a sample of three distinct fixture files, with the structural
/// variant file's transfer going as `failure` says.
fn exercise(output: Output, clock: &TestClock, failure: Failure, no_cache: bool) {
    // Keep each file distinct so ordering and cache use are observable
    let bytes = [
        b"small variant fixture\n".to_vec(),
        b"structural variant fixture\n".to_vec(),
        b"copy number fixture\n".to_vec(),
    ];
    let mut slots = sample_plan(&inventory(), sample("HG00096")).unwrap();
    for (slot, bytes) in slots.iter_mut().zip(&bytes) {
        let download = slot.download.as_mut().unwrap();
        download.bytes = bytes.len() as u64;
        download.sha256 = hex::encode(Sha256::digest(bytes));
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = if failure == Failure::None { 3 } else { 2 };
    let replies = bytes[..count]
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            let mut body = bytes.clone();
            if index == 1 {
                if failure == Failure::Digest {
                    body[0] ^= 1;
                }
                if failure == Failure::Size {
                    body.push(b'!');
                }
            }
            (200, String::new(), body)
        })
        .collect();
    let server = serve(listener, replies);
    let directory = Directory::new();
    let cache = (!no_cache).then_some(directory.0.as_path());

    // The Ark checks upload order and that each cached file is complete
    let starts = Arc::new(Mutex::new(Vec::new()));
    let processed = Arc::new(Mutex::new(Vec::new()));
    let mut peer = Peer::spawn(
        &clock.clock(),
        Box::new({
            let starts = starts.clone();
            let processed = processed.clone();
            let directory = directory.0.clone();
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
                    Content::SlotUploadStart(request) => {
                        let mut starts = starts.lock().unwrap();
                        let index = starts.len();
                        assert_eq!(request.kind, [3, 5, 6][index]);
                        assert_eq!(
                            request.name,
                            [
                                "HG00096.hard-filtered.vcf.gz",
                                "HG00096.sv.vcf.gz",
                                "HG00096.cnv.vcf.gz"
                            ][index]
                        );
                        assert_eq!(request.size, bytes[index].len() as u64);
                        assert_eq!(request.chunk, bytes[index]);
                        assert_eq!(processed.lock().unwrap().len(), index);
                        if !no_cache {
                            assert_eq!(
                                fs::read(
                                    directory.join(hex::encode(Sha256::digest(&bytes[index])))
                                )
                                .unwrap(),
                                bytes[index]
                            );
                        }
                        starts.push(request.kind);
                        if failure == Failure::Approval && index == 1 {
                            responder
                                .fail(
                                    schema::Error {
                                        code: 0x778,
                                        msg: "refused sample upload".into(),
                                    },
                                    deadline,
                                )
                                .unwrap();
                        } else {
                            responder
                                .reply(
                                    schema::SlotUploadStartResponse {
                                        session: request.kind as u64,
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }
                    }
                    Content::SlotUploadProcess(request) => {
                        let mut processed = processed.lock().unwrap();
                        assert_eq!(request.session, [3, 5, 6][processed.len()]);
                        processed.push(request.session);
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
                    _ => panic!("unexpected sample request: {request:?}"),
                }
                true
            }
        }),
    );
    let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
    let (ark, _) = Ark::attach(peer.stream(), &trust, |_| {
        Some((Environment::Develop, Realm::Hardware))
    })
    .unwrap();
    let context = Context {
        options: crate::args::Cli::parse_from(["ark"]).options,
        interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
        output,
    };

    // Use the real installer with loopback transport and small fixture pins
    let ordered: Vec<_> = slots.iter().collect();
    let result = execute(
        &context,
        &ordered,
        Some(sample("HG00096")),
        false,
        cache,
        |source| {
            let source = Source {
                url: format!("{url}/{}", source.dataset.name),
                dataset: source.dataset.clone(),
                hash: source.hash.clone(),
                personal: source.personal,
            };
            install(&context, &ark.client(), &source, cache, &agent())
        },
    );
    assert_eq!(result.is_err(), failure != Failure::None);
    if matches!(failure, Failure::Digest | Failure::Size) {
        assert_eq!(result.unwrap_err().code, "file-rejected");
    }
    assert_eq!(
        *starts.lock().unwrap(),
        match failure {
            Failure::None => vec![3, 5, 6],
            Failure::Approval => vec![3, 5],
            _ => vec![3],
        }
    );
    assert_eq!(
        *processed.lock().unwrap(),
        if failure == Failure::None {
            vec![3, 5, 6]
        } else {
            vec![3]
        }
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), count);
    for (request, suffix) in
        requests
            .iter()
            .zip(["hard-filtered.vcf.gz", "sv.vcf.gz", "cnv.vcf.gz"])
    {
        assert!(request.starts_with(&format!("get /hg00096.{suffix} http/1.1\r\n")));
    }
    if no_cache {
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }
    drop(ark);
}

/// Checks the approvals asked for and every row's outcome after a fetch whose
/// structural variant transfer went as `failure` says.
fn check(test: &str, failure: Failure, no_cache: bool) {
    for captured in crate::testing::narration(test, |output, clock| {
        exercise(output, clock, failure, no_cache)
    }) {
        let approvals = match failure {
            Failure::None => 3,
            Failure::Approval => 2,
            _ => 1,
        };
        captured.approvals(&vec![("upload", Some("phone")); approvals]);
        if captured.json {
            let document: Value = serde_json::from_str(&captured.stdout).unwrap();
            let rows = document["fetched"].as_array().unwrap();
            assert_eq!(rows.len(), 3);
            for (index, (row, (id, slot, suffix, _, _))) in rows.iter().zip(FILES).enumerate() {
                assert_eq!(row["slot"], slot);
                assert_eq!(row["id"], id);
                assert!(
                    row["url"]
                        .as_str()
                        .unwrap()
                        .ends_with(&format!("/HG00096/HG00096.{suffix}"))
                );
                assert_eq!(row["sample"], "HG00096");
                assert_eq!(row["name"], "notable-owen");
                assert_eq!(row["cached"], false);
                assert_eq!(
                    row["outcome"],
                    if failure == Failure::None || index == 0 {
                        "done"
                    } else if index == 1 {
                        "failed"
                    } else {
                        "not-attempted"
                    }
                );
                assert_eq!(
                    row["error"].is_null(),
                    failure == Failure::None || index != 1
                );
            }
        }
    }
}

/// Checks that each file downloads and uploads with its own approval.
#[test]
fn test_sample_uploads_three_files_in_order() {
    check(
        "data::reference::tests::samples::test_sample_uploads_three_files_in_order",
        Failure::None,
        false,
    );
}

/// Checks that a structural variant file with the wrong digest stops the fetch
/// before the copy numbers.
#[test]
fn test_sample_stops_on_structural_digest_failure() {
    check(
        "data::reference::tests::samples::test_sample_stops_on_structural_digest_failure",
        Failure::Digest,
        false,
    );
}

/// Checks that an oversized structural variant file stops before its approval.
#[test]
fn test_sample_stops_on_structural_size_failure() {
    check(
        "data::reference::tests::samples::test_sample_stops_on_structural_size_failure",
        Failure::Size,
        false,
    );
}

/// Checks that a refused structural variant upload is never retried.
#[test]
fn test_sample_stops_on_structural_upload_failure() {
    check(
        "data::reference::tests::samples::test_sample_stops_on_structural_upload_failure",
        Failure::Approval,
        false,
    );
}

/// Checks that --no-cache leaves none of the three temporary files behind,
/// on success or failure.
#[test]
fn test_sample_no_cache_leaves_no_files() {
    let test = "data::reference::tests::samples::test_sample_no_cache_leaves_no_files";
    if let Ok(scenario) = std::env::var("ARK_TEST_SAMPLE_TEMP") {
        let failure = if scenario == "success" {
            Failure::None
        } else {
            Failure::Approval
        };
        check(test, failure, true);
        return;
    }
    for scenario in ["success", "failure"] {
        let directory = Directory::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("ARK_TEST_SAMPLE_TEMP", scenario)
            .env("TMPDIR", &directory.0)
            .env("TMP", &directory.0)
            .env("TEMP", &directory.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "{scenario}: {output:?}");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0, "{scenario}");
    }
}
