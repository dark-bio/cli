// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Fetches into personal slots, against the scripted HTTP server and Ark.

use super::*;
use crate::testing::{Narration, peer::Peer};
use darkbio_connect::{
    Ark, TrustMode, schema,
    trust::{Environment, Realm},
};
use schema::host_to_ark::Content;
use std::sync::{Arc, Mutex};

/// A condition of the download, the cache or the upload under test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// The download breaks after two cache chunks and a few bytes more.
    Resume,
    /// The cached copy has the right size and the wrong SHA-256.
    CorruptCache,
    /// The cached copy has the wrong size.
    ShortCache,
    /// The Ark refuses a chunk after the upload is approved.
    UploadFailure,
    /// The fetch succeeds without a cache.
    NoCache,
    /// The approved upload fails without a cache.
    NoCacheFailure,
    /// A file sits where the cache directory should be.
    Unavailable,
    /// Another download holds the cache entry's lock.
    InUse,
    /// The whole file downloads with the wrong SHA-256.
    CorruptDownload,
    /// All three attempts break before the file ends.
    DownloadFailure,
    /// The cache holds a verified copy, so nothing downloads.
    Cached,
    /// The Ark reports a reference kind as a personal slot.
    PersonalOrigin,
}

/// Runs a scenario in every output mode and checks what each narrates.
fn check(test: &str, scenario: Scenario) {
    for captured in
        crate::testing::narration(test, |output, clock| exercise(output, clock, scenario))
    {
        check_output(&captured, scenario);
    }
}

/// Checks that a verified download precedes one approval, and a bad one none.
fn check_output(captured: &Narration, scenario: Scenario) {
    let invalid = matches!(
        scenario,
        Scenario::CorruptDownload | Scenario::DownloadFailure
    );
    if invalid {
        captured.approvals(&[]);
        assert!(
            !captured.stderr.contains("Approve the upload"),
            "invalid download asks no approval"
        );
    } else {
        captured.approvals(&[("upload", Some("phone"))]);
        if scenario != Scenario::Cached {
            captured.contains(&[
                "Downloaded ",
                "Approve the upload on your phone, in Ark Companion",
            ]);
        }
        assert_eq!(
            captured
                .stderr
                .matches("Approve the upload on your phone, in Ark Companion")
                .count(),
            1,
            "one approval instruction"
        );
    }
    if matches!(scenario, Scenario::CorruptCache | Scenario::ShortCache) {
        captured.contains(&[
            "cached download failed verification; downloading it again",
            "Downloading",
            "Approve the upload",
        ]);
    }
    if scenario == Scenario::Resume {
        captured.contains(&[
            "download interrupted; retry 2/3",
            "Downloaded",
            "Approve the upload",
        ]);
    }
    if scenario == Scenario::Cached {
        assert!(
            !captured.stderr.contains("Downloading"),
            "verified cache skips the network stage"
        );
    }
}

/// Runs the real installer on a file larger than an upload chunk.
fn exercise(output: Output, clock: &mut TestClock, scenario: Scenario) {
    // Pin a made-up file longer than the identification head and an upload
    // chunk
    let bytes: Vec<_> = (0..3 * cache::CHUNK + 37)
        .map(|index| (index % 251) as u8)
        .collect();
    let digest = hex::encode(Sha256::digest(&bytes));
    let mut slot = sample_slot(sample("HG00096"));
    if scenario == Scenario::PersonalOrigin {
        slot.kind = SlotKind::VariantCatalog as i32;
    }
    let download = slot.download.as_mut().unwrap();
    download.bytes = bytes.len() as u64;
    download.sha256 = digest.clone();
    let mut source = offer(&slot).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    source.url = format!("http://{}/sample.vcf.gz", listener.local_addr().unwrap());
    let directory = Directory::new();
    let path = directory.0.join(&digest);

    // Set up the cache with the files and entry locks the fetch itself uses
    match scenario {
        Scenario::CorruptCache => fs::write(&path, vec![0; bytes.len()]).unwrap(),
        Scenario::ShortCache => fs::write(&path, &bytes[..cache::CHUNK]).unwrap(),
        Scenario::Cached => fs::write(&path, &bytes).unwrap(),
        Scenario::Unavailable => fs::write(directory.0.join("unavailable"), b"file").unwrap(),
        _ => {}
    }
    let held = (scenario == Scenario::InUse).then(|| {
        cache::Entry::open(&directory.0, &digest, &source.url, source.dataset.size).unwrap()
    });
    let unavailable = directory.0.join("unavailable");
    let cache = match scenario {
        Scenario::NoCache | Scenario::NoCacheFailure => None,
        Scenario::Unavailable => Some(unavailable.as_path()),
        _ => Some(directory.0.as_path()),
    };
    let retained = !matches!(
        scenario,
        Scenario::NoCache | Scenario::NoCacheFailure | Scenario::Unavailable | Scenario::InUse
    );

    // Break the first response midway, then serve only the bytes after the
    // kept prefix
    let replies = match scenario {
        Scenario::Resume => vec![
            (200, String::new(), bytes[..2 * cache::CHUNK + 17].to_vec()),
            (
                206,
                format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    2 * cache::CHUNK,
                    bytes.len() - 1,
                    bytes.len()
                ),
                bytes[2 * cache::CHUNK..].to_vec(),
            ),
        ],
        Scenario::DownloadFailure => vec![
            (200, String::new(), bytes[..cache::CHUNK].to_vec()),
            (
                206,
                format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    cache::CHUNK,
                    bytes.len() - 1,
                    bytes.len()
                ),
                bytes[cache::CHUNK..2 * cache::CHUNK].to_vec(),
            ),
            (
                206,
                format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    2 * cache::CHUNK,
                    bytes.len() - 1,
                    bytes.len()
                ),
                bytes[2 * cache::CHUNK..3 * cache::CHUNK].to_vec(),
            ),
        ],
        Scenario::CorruptDownload => vec![(200, String::new(), vec![0; bytes.len()])],
        Scenario::Cached => vec![],
        _ => vec![(200, String::new(), bytes.clone())],
    };
    let expected_requests = replies.len();
    let server = serve(listener, replies);

    // Count upload starts on the Ark, which refuses a later chunk for an upload
    // failure
    let starts = Arc::new(Mutex::new(0));
    let received = Arc::new(Mutex::new(Vec::new()));
    let failed_upload = matches!(scenario, Scenario::UploadFailure | Scenario::NoCacheFailure);
    let invalid = matches!(
        scenario,
        Scenario::CorruptDownload | Scenario::DownloadFailure
    );
    let mut peer = Peer::spawn(
        &clock.clock(),
        Box::new({
            let starts = starts.clone();
            let received = received.clone();
            let expected = bytes.clone();
            let path = path.clone();
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
                        *starts.lock().unwrap() += 1;
                        assert!(!invalid, "unverified bytes must never open an upload");
                        if retained {
                            assert_eq!(
                                fs::read(&path).unwrap(),
                                expected,
                                "whole verified cache exists before approval"
                            );
                        }
                        assert_eq!(request.kind, slot.kind, "Ark's target slot");
                        assert_eq!(request.size, expected.len() as u64, "pinned upload length");
                        assert_eq!(request.name, "HG00096.hard-filtered.vcf.gz", "sample name");
                        received.lock().unwrap().extend(request.chunk);
                        responder
                            .reply(schema::SlotUploadStartResponse { session: 7 }, deadline)
                            .unwrap();
                    }
                    Content::SlotUploadChunk(request) => {
                        assert_eq!(request.session, 7, "only upload session");
                        if failed_upload {
                            responder
                                .fail(
                                    schema::Error {
                                        code: 0x778,
                                        msg: "refused upload chunk".into(),
                                    },
                                    deadline,
                                )
                                .unwrap();
                        } else {
                            received.lock().unwrap().extend(request.chunk);
                            responder
                                .reply(schema::SlotUploadChunkResponse {}, deadline)
                                .unwrap();
                        }
                    }
                    Content::SlotUploadProcess(request) => {
                        assert!(!failed_upload, "failed upload cannot process");
                        assert_eq!(request.session, 7, "processing session");
                        assert_eq!(
                            *received.lock().unwrap(),
                            expected,
                            "every downloaded byte reaches the Ark"
                        );
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
                    Content::SlotUploadCancel(request) => {
                        assert!(failed_upload, "only failed uploads need cancellation");
                        assert_eq!(request.session, 7, "cancelled upload");
                        responder
                            .reply(schema::SlotUploadCancelResponse {}, deadline)
                            .unwrap();
                    }
                    _ => panic!("unexpected request: {request:?}"),
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
    let result = install(&context, &ark.client(), &source, cache, &agent());

    // A refused upload keeps the cache, and a bad download never uploads
    assert_eq!(
        result.is_err(),
        failed_upload || invalid,
        "scenario result: {result:?}"
    );
    assert_eq!(
        *starts.lock().unwrap(),
        if invalid { 0 } else { 1 },
        "approval-bearing upload starts"
    );
    if retained && !invalid {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "verified copy survives upload outcome"
        );
    } else {
        assert!(!path.exists(), "no complete cache entry");
    }
    if scenario == Scenario::CorruptDownload {
        assert_eq!(
            result.unwrap_err().code,
            "file-rejected",
            "bad digest rejected locally"
        );
        assert!(
            !directory.0.join(format!("{digest}.part")).exists(),
            "bad complete download discarded"
        );
    }
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        expected_requests,
        "bounded network attempts"
    );
    if scenario == Scenario::Resume {
        assert!(
            requests[1].contains("\r\nrange: bytes=2097152-\r\n"),
            "resume from whole cache chunks"
        );
        assert!(
            requests[1].contains("\r\nif-range: \"reference-v1\"\r\n"),
            "resume validator"
        );
    }
    drop(held);
    drop(ark);
}

/// A resumed download larger than an upload chunk asks for one approval.
#[test]
fn test_resume_precedes_one_approval() {
    check(
        "data::reference::tests::personal::test_resume_precedes_one_approval",
        Scenario::Resume,
    );
}

/// A cached copy with the wrong SHA-256 is dropped before any upload.
#[test]
fn test_corrupt_cache_precedes_one_approval() {
    check(
        "data::reference::tests::personal::test_corrupt_cache_precedes_one_approval",
        Scenario::CorruptCache,
    );
}

/// A cached copy with the wrong size is dropped before any upload.
#[test]
fn test_short_cache_precedes_one_approval() {
    check(
        "data::reference::tests::personal::test_short_cache_precedes_one_approval",
        Scenario::ShortCache,
    );
}

/// An upload that fails after approval keeps the verified copy and is not
/// retried.
#[test]
fn test_failed_upload_keeps_verified_cache() {
    check(
        "data::reference::tests::personal::test_failed_upload_keeps_verified_cache",
        Scenario::UploadFailure,
    );
}

/// A verified cached copy uploads once, without a download.
#[test]
fn test_verified_cache_skips_download() {
    check(
        "data::reference::tests::personal::test_verified_cache_skips_download",
        Scenario::Cached,
    );
}

/// The Ark's personal origin picks the download-first path, whatever the kind.
#[test]
fn test_origin_selects_personal_staging() {
    check(
        "data::reference::tests::personal::test_origin_selects_personal_staging",
        Scenario::PersonalOrigin,
    );
}

/// A whole download with the wrong SHA-256 is dropped without an approval.
#[test]
fn test_bad_download_never_asks_for_approval() {
    check(
        "data::reference::tests::personal::test_bad_download_never_asks_for_approval",
        Scenario::CorruptDownload,
    );
}

/// Three broken downloads give up without opening an upload.
#[test]
fn test_exhausted_download_never_asks_for_approval() {
    check(
        "data::reference::tests::personal::test_exhausted_download_never_asks_for_approval",
        Scenario::DownloadFailure,
    );
}

/// No temporary file outlives a success, a refusal or an unusable cache.
#[test]
fn test_temporary_download_cleanup() {
    // Each child gets its own temp directory, which must be empty once it exits
    let test = "data::reference::tests::personal::test_temporary_download_cleanup";
    if let Ok(scenario) = std::env::var("ARK_TEST_PERSONAL_TEMP") {
        let scenario = match scenario.as_str() {
            "success" => Scenario::NoCache,
            "failure" => Scenario::NoCacheFailure,
            "unavailable" => Scenario::Unavailable,
            "in-use" => Scenario::InUse,
            _ => panic!("unknown temporary download scenario"),
        };
        check(test, scenario);
        return;
    }
    for scenario in ["success", "failure", "unavailable", "in-use"] {
        let directory = Directory::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("ARK_TEST_PERSONAL_TEMP", scenario)
            .env("ARK_TEST_COMMAND_NARRATION", "json")
            .env("TMPDIR", &directory.0)
            .env("TMP", &directory.0)
            .env("TEMP", &directory.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "{scenario}: {output:?}");
        assert_eq!(
            fs::read_dir(&directory.0).unwrap().count(),
            0,
            "{scenario} leaves no temporary file"
        );
        let captured = Narration {
            json: true,
            terminal: false,
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        };
        check_output(
            &captured,
            if scenario == "failure" {
                Scenario::NoCacheFailure
            } else {
                Scenario::NoCache
            },
        );
    }
}
