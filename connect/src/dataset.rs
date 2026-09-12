// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dataset identification, streaming and processing on the Ark.

use crate::{Error, Timing, schema};
use darkbio_wire::protocol::{Message, Promise, Requester};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::time::{Duration, Instant};

const IDENTIFY_SIZE: usize = 1024 * 1024;
const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024; // Leave room for sealing and framing
const CHUNK_INTERVAL: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Upload stages reported on the caller's thread. Acknowledged bytes may still
/// need writing or validation; only successful processing completes the upload.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadProgress {
    /// Asking the Ark to identify the file from its first chunk.
    Identifying,
    /// The Ark's identification, including its summary and confidence.
    Identified(schema::SlotIdentifyResponse),
    /// Opening an upload session. The Ark may request companion approval.
    Preparing,
    /// The upload session is available for explicit cancellation.
    Started { session: u64 },
    /// Dataset bytes acknowledged by the Ark so far.
    Uploading { uploaded: u64, total: u64 },
    /// Validation and indexing progress reported by the Ark.
    Processing(schema::SlotUploadProcessResponse),
}

/// A local file needs identification; a reference already names its target
/// slot and carries the hash advertised alongside the download.
#[derive(Clone, Debug)]
pub struct Dataset {
    pub name: String,
    pub size: u64,
    /// Target slot, or None to let the Ark identify the file.
    pub slot: Option<i32>,
    /// Optional SHA-256 checked before processing.
    pub sha256: Option<[u8; 32]>,
}

/// Identifies once, resends that same head in the authorized start request and
/// streams the rest. A failed session is cancelled within the remaining deadline;
/// cleanup never replaces the original error or retries an upload.
pub(crate) fn upload(
    requester: &Requester,
    dataset: &Dataset,
    reader: &mut impl Read,
    timing: impl Into<Timing>,
    mut progress: impl FnMut(UploadProgress),
) -> Result<(), Error> {
    let timing = timing.into();
    if dataset.size == 0 {
        return Err(Error::Dataset("dataset is empty".into()));
    }
    let head = read_chunk(
        reader,
        dataset.size.min(IDENTIFY_SIZE as u64) as usize,
        timing,
        None,
    )?;
    let mut hash = dataset.sha256.map(|_| Sha256::new());
    if let Some(hash) = &mut hash {
        hash.update(&head);
    }
    if head.len() as u64 == dataset.size {
        finish_read(reader, hash.take(), dataset, timing)?;
    }
    let kind = match dataset.slot {
        Some(kind) => kind,
        None => {
            progress(UploadProgress::Identifying);
            let identified = requester
                .request(
                    schema::SlotIdentifyRequest {
                        name: dataset.name.clone(),
                        size: dataset.size,
                        chunk: head.clone(),
                        kinds: Vec::new(),
                    },
                    timing.io(),
                )?
                .wait::<schema::SlotIdentifyResponse>()?;
            if !identified.rejection.is_empty() {
                return Err(Error::Dataset(identified.rejection));
            }
            let kind = identified.kind;
            progress(UploadProgress::Identified(identified));
            kind
        }
    };
    progress(UploadProgress::Preparing);
    let mut uploaded = head.len() as u64;
    let session = requester
        .request(
            schema::SlotUploadStartRequest {
                kind,
                name: dataset.name.clone(),
                size: dataset.size,
                chunk: head,
            },
            timing.approval(),
        )?
        .wait::<schema::SlotUploadStartResponse>()?
        .session;
    let result = (|| {
        progress(UploadProgress::Started { session });
        progress(UploadProgress::Uploading {
            uploaded,
            total: dataset.size,
        });
        let mut sent = uploaded;
        let mut pending: Option<(Promise<Message>, u64)> = None;
        while sent < dataset.size {
            let size = (dataset.size - sent).min(CHUNK_SIZE as u64) as usize;
            let chunk = read_chunk(reader, size, timing, Some(CHUNK_INTERVAL))?;
            let size = chunk.len() as u64;
            if let Some(hash) = &mut hash {
                hash.update(&chunk);
            }
            sent += size;
            if sent == dataset.size {
                finish_read(reader, hash.take(), dataset, timing)?;
            }
            // Keep at most two chunks outstanding so device writes can overlap
            // the next transfer. The last acknowledgement is awaited too.
            let next = requester.request(
                schema::SlotUploadChunkRequest { session, chunk },
                timing.io(),
            )?;
            if let Some((previous, bytes)) = pending.take() {
                previous.wait::<schema::SlotUploadChunkResponse>()?;
                uploaded += bytes;
                progress(UploadProgress::Uploading {
                    uploaded,
                    total: dataset.size,
                });
            }
            pending = Some((next, size));
        }
        if let Some((last, bytes)) = pending {
            last.wait::<schema::SlotUploadChunkResponse>()?;
            uploaded += bytes;
            progress(UploadProgress::Uploading {
                uploaded,
                total: dataset.size,
            });
        }
        loop {
            let status = requester
                .request(schema::SlotUploadProcessRequest { session }, timing.io())?
                .wait::<schema::SlotUploadProcessResponse>()?;
            if !status.failure.is_empty() {
                return Err(Error::Dataset(status.failure));
            }
            // An empty or malformed report must not turn into false success.
            let phases = status.phases.len() as u64;
            if phases == 0
                || status.phase_in == 0
                || status.phase_in > phases
                || status.phase_progress > 10_000
            {
                return Err(Error::Dataset("invalid dataset processing progress".into()));
            }
            let done = status.phase_in == phases && status.phase_progress == 10_000;
            progress(UploadProgress::Processing(status));
            if done {
                return Ok(());
            }
            timing.pause(POLL_INTERVAL)?;
        }
    })();
    if result.is_err() {
        let cleanup = timing.io().min(Instant::now() + Duration::from_secs(1));
        let _ = requester
            .request(schema::SlotUploadCancelRequest { session }, cleanup)
            .and_then(|pending| pending.wait::<schema::SlotUploadCancelResponse>());
    }
    result
}

/// Fill the identification head before opening a session. Later chunks flush
/// available bytes periodically so a slow source keeps the Ark's session alive.
/// The caller's reader still owns the timeout of each individual read.
fn read_chunk(
    reader: &mut impl Read,
    size: usize,
    timing: Timing,
    interval: Option<Duration>,
) -> Result<Vec<u8>, Error> {
    let start = Instant::now();
    let mut chunk = vec![0; size];
    let mut filled = 0;
    while filled < size {
        timing.check()?;
        match reader.read(&mut chunk[filled..]) {
            Ok(0) => return Err(read_error(io::ErrorKind::UnexpectedEof.into())),
            Ok(count) => filled += count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(read_error(err)),
        }
        if interval.is_some_and(|interval| start.elapsed() >= interval) {
            break;
        }
    }
    timing.check()?;
    chunk.truncate(filled);
    Ok(chunk)
}

/// Checks EOF and the advertised hash before sending the final chunk. Earlier
/// chunks may already be accepted, but a bad source never reaches processing.
fn finish_read(
    reader: &mut impl Read,
    hash: Option<Sha256>,
    dataset: &Dataset,
    timing: Timing,
) -> Result<(), Error> {
    loop {
        timing.check()?;
        match reader.read(&mut [0]) {
            Ok(0) => break,
            Ok(_) => {
                return Err(Error::Integrity(
                    "dataset exceeds its advertised size".into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_error(error)),
        }
    }
    timing.check()?;
    if let (Some(hash), Some(expected)) = (hash, dataset.sha256)
        && hash.finalize().as_slice() != expected
    {
        return Err(Error::Integrity(
            "reference SHA-256 does not match the advertised dataset".into(),
        ));
    }
    Ok(())
}

fn read_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut {
        Error::Timeout
    } else {
        Error::DatasetRead(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrustMode;
    use crate::testing::Peer;
    use darkbio_wire::protocol::{self, Session};
    use schema::host_to_ark::Content;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct Observed {
        stages: Vec<&'static str>,
        head: Vec<u8>,
        bytes: Vec<u8>,
        kind: Option<i32>,
    }

    fn status(phase: u64, progress: u64) -> schema::SlotUploadProcessResponse {
        schema::SlotUploadProcessResponse {
            phases: ["Validate", "Index"]
                .map(|name| schema::SlotPhase {
                    name: name.into(),
                    desc: String::new(),
                })
                .into(),
            phase_in: phase,
            phase_progress: progress,
            ..Default::default()
        }
    }

    fn source(size: usize, reference: Option<[u8; 32]>) -> Dataset {
        Dataset {
            name: "sample.vcf.gz".into(),
            size: size as u64,
            slot: reference.map(|_| 3),
            sha256: reference,
        }
    }

    fn attach(peer: &mut Peer) -> Session {
        let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
        protocol::connect(peer.stream(), &trust).unwrap().0
    }

    /// Records the actual wire exchange, optionally refusing a stage. A cancel
    /// refusal must never replace the failure that caused cleanup.
    fn peer(
        fail: Option<&'static str>,
        reports: Vec<schema::SlotUploadProcessResponse>,
    ) -> (Peer, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = observed.clone();
        let mut reports = VecDeque::from(reports);
        let peer = Peer::spawn(Box::new(move |session, request, responder| {
            if matches!(request, Content::DeviceInfo(_)) {
                return crate::testing::answering(session, request, responder);
            }
            let deadline = Instant::now() + TIMEOUT;
            let mut observed = shared.lock().unwrap();
            let (stage, response): (_, Message) = match request {
                Content::CloudSyncStart(_) => (
                    "sync-start",
                    schema::CloudSyncStartResponse { challenge: vec![3] }.into(),
                ),
                Content::CloudSyncFinish(_) => (
                    "sync-finish",
                    schema::CloudSyncFinishResponse { accepted: 123 }.into(),
                ),
                Content::SlotIdentify(request) => {
                    assert_eq!(request.name, "sample.vcf.gz");
                    assert!(request.kinds.is_empty());
                    observed.head = request.chunk;
                    (
                        "peek",
                        schema::SlotIdentifyResponse {
                            kind: 2,
                            summary: "Variant calls".into(),
                            rejection: if fail == Some("identify") {
                                "unrecognized dataset".into()
                            } else {
                                String::new()
                            },
                            ..Default::default()
                        }
                        .into(),
                    )
                }
                Content::SlotUploadStart(request) => {
                    assert_eq!(request.name, "sample.vcf.gz");
                    if !observed.head.is_empty() {
                        assert_eq!(request.chunk, observed.head);
                    }
                    observed.bytes.extend(request.chunk);
                    observed.kind = Some(request.kind);
                    (
                        "start",
                        schema::SlotUploadStartResponse { session: 7 }.into(),
                    )
                }
                Content::SlotUploadChunk(request) => {
                    assert_eq!(request.session, 7);
                    assert!(request.chunk.len() <= CHUNK_SIZE);
                    observed.bytes.extend(request.chunk);
                    ("chunk", schema::SlotUploadChunkResponse {}.into())
                }
                Content::SlotUploadProcess(request) => {
                    assert_eq!(request.session, 7);
                    (
                        "process",
                        reports
                            .pop_front()
                            .unwrap_or_else(|| status(2, 10_000))
                            .into(),
                    )
                }
                Content::SlotUploadCancel(request) => {
                    assert_eq!(request.session, 7);
                    ("cancel", schema::SlotUploadCancelResponse {}.into())
                }
                _ => panic!("unexpected request"),
            };
            observed.stages.push(stage);
            if fail == Some(stage) || stage == "cancel" && fail.is_some() {
                responder
                    .fail(
                        schema::Error {
                            code: 0x778,
                            msg: format!("refused {stage}"),
                        },
                        deadline,
                    )
                    .unwrap();
            } else {
                responder.reply(response, deadline).unwrap();
            }
            true
        }));
        (peer, observed)
    }

    /// Start includes the same head used for identification. Remaining chunks
    /// arrive exactly once, and an early phase reaching 100% is not completion.
    #[test]
    fn test_upload() {
        for size in [17, IDENTIFY_SIZE, IDENTIFY_SIZE + 2 * CHUNK_SIZE + 29] {
            let bytes: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let (mut peer, observed) = peer(None, vec![status(1, 10_000), status(2, 10_000)]);
            let session = attach(&mut peer);
            let mut progress = Vec::new();
            upload(
                &session.requester(),
                &source(size, None),
                &mut bytes.as_slice(),
                Instant::now() + TIMEOUT,
                |stage| progress.push(stage),
            )
            .unwrap();
            let observed = observed.lock().unwrap();
            assert_eq!(observed.bytes, bytes);
            assert_eq!(observed.kind, Some(2));
            assert_eq!(
                observed
                    .stages
                    .iter()
                    .filter(|&&stage| stage == "process")
                    .count(),
                2
            );
            assert!(!observed.stages.contains(&"cancel"));
            assert!(progress.contains(&UploadProgress::Uploading {
                uploaded: size as u64,
                total: size as u64
            }));
            assert!(
                matches!(progress.last(), Some(UploadProgress::Processing(report)) if report.phase_in == 2)
            );
        }
    }

    /// A slow source must send a partial chunk before reading the rest. The
    /// reader models a source that only continues once the Ark receives it.
    #[test]
    fn test_slow_source_flushes_partial_chunks() {
        struct Slow<'a> {
            bytes: &'a [u8],
            reads: usize,
            observed: Arc<Mutex<Observed>>,
        }
        impl Read for Slow<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads == 2 {
                    std::thread::sleep(CHUNK_INTERVAL);
                } else if self.reads == 3 {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while self.observed.lock().unwrap().bytes.len() <= IDENTIFY_SIZE {
                        if Instant::now() >= deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        std::thread::yield_now();
                    }
                }
                let size = if self.reads == 1 {
                    IDENTIFY_SIZE
                } else {
                    64 * 1024
                };
                let size = size.min(buffer.len());
                self.bytes.read(&mut buffer[..size])
            }
        }
        let bytes = vec![42; IDENTIFY_SIZE + 128 * 1024];
        let (mut peer, observed) = peer(None, vec![]);
        let session = attach(&mut peer);
        let mut reader = Slow {
            bytes: &bytes,
            reads: 0,
            observed: observed.clone(),
        };
        upload(
            &session.requester(),
            &source(bytes.len(), None),
            &mut reader,
            Instant::now() + TIMEOUT,
            |_| {},
        )
        .unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.bytes, bytes);
        assert_eq!(
            observed
                .stages
                .iter()
                .filter(|&&stage| stage == "chunk")
                .count(),
            2
        );
    }

    /// A successful transfer may still fail validation. Neither a refusal nor a
    /// malformed progress report can be mistaken for completed processing.
    #[test]
    fn test_failures() {
        for fail in ["identify", "peek", "start", "chunk", "process"] {
            let bytes = vec![42; IDENTIFY_SIZE + 2 * CHUNK_SIZE + 1];
            let (mut peer, observed) = peer(Some(fail), vec![]);
            let session = attach(&mut peer);
            let result = upload(
                &session.requester(),
                &source(bytes.len(), None),
                &mut bytes.as_slice(),
                Instant::now() + TIMEOUT,
                |_| {},
            );
            if fail == "identify" {
                assert!(
                    matches!(result, Err(Error::Dataset(reason)) if reason == "unrecognized dataset")
                );
            } else {
                assert!(
                    matches!(result, Err(Error::Remote(error)) if error.msg == format!("refused {fail}"))
                );
            }
            let observed = observed.lock().unwrap();
            assert_eq!(
                observed.stages.contains(&"cancel"),
                matches!(fail, "chunk" | "process")
            );
            assert_eq!(
                observed
                    .stages
                    .iter()
                    .filter(|&&stage| stage == "start")
                    .count(),
                usize::from(!matches!(fail, "identify" | "peek"))
            );
            if fail == "chunk" {
                assert!(!observed.stages.contains(&"process"));
            }
        }
        for report in [
            schema::SlotUploadProcessResponse {
                failure: "invalid genome".into(),
                ..status(2, 10_000)
            },
            schema::SlotUploadProcessResponse::default(),
            status(0, 0),
            status(3, 10_000),
            status(2, 10_001),
        ] {
            let (mut peer, observed) = peer(None, vec![report]);
            let session = attach(&mut peer);
            let result = upload(
                &session.requester(),
                &source(1, None),
                &mut [42].as_slice(),
                Instant::now() + TIMEOUT,
                |_| {},
            );
            assert!(matches!(result, Err(Error::Dataset(_))));
            assert_eq!(observed.lock().unwrap().stages.last(), Some(&"cancel"));
        }
    }

    /// References bypass identification and retain their advertised kind. A
    /// bad length or hash prevents processing, for both small and large files.
    #[test]
    fn test_integrity() {
        for size in [17, IDENTIFY_SIZE + CHUNK_SIZE + 17] {
            let original = vec![42; size];
            let hash = Sha256::digest(&original).into();
            for change in ["none", "short", "long", "corrupt"] {
                let mut bytes = original.clone();
                match change {
                    "short" => {
                        bytes.pop();
                    }
                    "long" => bytes.push(42),
                    "corrupt" => bytes[size - 1] ^= 1,
                    _ => {}
                }
                let (mut peer, observed) = peer(None, vec![]);
                let session = attach(&mut peer);
                let result = upload(
                    &session.requester(),
                    &source(size, Some(hash)),
                    &mut bytes.as_slice(),
                    Instant::now() + TIMEOUT,
                    |_| {},
                );
                let observed = observed.lock().unwrap();
                assert!(!observed.stages.contains(&"peek"));
                assert_eq!(result.is_ok(), change == "none");
                if change == "none" {
                    assert_eq!(observed.kind, Some(3));
                    assert_eq!(observed.bytes, original);
                } else {
                    assert!(!observed.stages.contains(&"process"));
                    assert_eq!(observed.stages.contains(&"cancel"), size > IDENTIFY_SIZE);
                }
            }
        }
    }

    /// Two chunks may be queued, but a third cannot precede their acknowledgement.
    /// Responses arriving in reverse order must not advance the source early.
    #[test]
    fn test_transfer_window() {
        let (notice, notices) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let mut held = None;
        let mut chunks = 0;
        let mut peer = Peer::spawn(Box::new(move |_, request, responder| {
            let deadline = Instant::now() + TIMEOUT;
            match request {
                Content::SlotUploadStart(_) => {
                    responder
                        .reply(schema::SlotUploadStartResponse { session: 7 }, deadline)
                        .unwrap();
                }
                Content::SlotUploadChunk(_) => {
                    chunks += 1;
                    if chunks == 1 {
                        held = Some(responder);
                    } else {
                        responder
                            .reply(schema::SlotUploadChunkResponse {}, deadline)
                            .unwrap();
                        if chunks == 2 {
                            notice.send(()).unwrap();
                            released.recv_timeout(TIMEOUT).unwrap();
                            held.take()
                                .unwrap()
                                .reply(schema::SlotUploadChunkResponse {}, deadline)
                                .unwrap();
                        }
                    }
                }
                Content::SlotUploadProcess(_) => {
                    responder.reply(status(2, 10_000), deadline).unwrap();
                }
                _ => panic!("unexpected request"),
            }
            true
        }));
        let session = attach(&mut peer);
        let bytes = vec![42; IDENTIFY_SIZE + 3 * CHUNK_SIZE];
        let source = source(bytes.len(), Some(Sha256::digest(&bytes).into()));
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counting<'a> {
            bytes: &'a [u8],
            count: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Read for Counting<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(buf)?;
                self.count
                    .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                Ok(count)
            }
        }
        let counted = read.clone();
        let requester = session.requester();
        let worker = std::thread::spawn(move || {
            upload(
                &requester,
                &source,
                &mut Counting {
                    bytes: &bytes,
                    count: counted,
                },
                Instant::now() + TIMEOUT,
                |_| {},
            )
        });
        notices.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(
            read.load(std::sync::atomic::Ordering::SeqCst),
            IDENTIFY_SIZE + 2 * CHUNK_SIZE
        );
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    /// Caller-owned readers may yield short reads or interrupted syscalls.
    #[test]
    fn test_reader_errors() {
        struct Fragmented {
            calls: usize,
            bytes: &'static [u8],
        }
        impl Read for Fragmented {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls % 2 == 1 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let size = buf.len().min(1);
                self.bytes.read(&mut buf[..size])
            }
        }
        let (mut peer, _) = peer(None, vec![]);
        let session = attach(&mut peer);
        let mut reader = Fragmented {
            calls: 0,
            bytes: b"hello",
        };
        upload(
            &session.requester(),
            &source(5, None),
            &mut reader,
            Instant::now() + TIMEOUT,
            |_| {},
        )
        .unwrap();
        assert!(matches!(
            read_error(io::ErrorKind::TimedOut.into()),
            Error::Timeout
        ));
    }

    /// The public helper establishes cloud setup once across repeated uploads.
    /// It does not eagerly attach a relay when the Ark needs no approval.
    #[test]
    fn test_client_setup() {
        use crate::cloud::tests::{response, serve};
        let (url, requests) = serve(vec![
            (
                Duration::ZERO,
                response(200, r#"{"signer":"AQ==","crypto":"Ag=="}"#),
            ),
            (
                Duration::ZERO,
                response(200, r#"{"unixmilli":123,"signature":"BA=="}"#),
            ),
        ]);
        let (mut peer, observed) = peer(None, vec![]);
        let ark = crate::cloud::tests::attach(&mut peer, url);
        for _ in 0..2 {
            ark.client()
                .upload_dataset(
                    &source(1, None),
                    &mut [42].as_slice(),
                    Instant::now() + TIMEOUT,
                    |_| {},
                )
                .unwrap();
        }
        let observed = observed.lock().unwrap();
        assert_eq!(
            observed
                .stages
                .iter()
                .filter(|&&stage| stage == "sync-start")
                .count(),
            1
        );
        assert_eq!(
            &observed.stages[..3],
            &["sync-start", "sync-finish", "peek"]
        );
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .contains("/cloudsync/identity")
        );
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .contains("/cloudsync/time?challenge=03")
        );
    }
    /// The processing operation may outlive one wait allowance, even when
    /// successive replies report the same progress percentage.
    #[test]
    fn test_processing_renews_waits() {
        let (mut peer, observed) = peer(
            None,
            vec![status(1, 100), status(1, 100), status(2, 10_000)],
        );
        let session = attach(&mut peer);
        let started = Instant::now();
        upload(
            &session.requester(),
            &source(1, None),
            &mut [42].as_slice(),
            Timing::inactivity(Duration::from_millis(250)),
            |_| {},
        )
        .unwrap();
        assert!(started.elapsed() >= 2 * POLL_INTERVAL);
        assert!(!observed.lock().unwrap().stages.contains(&"cancel"));
    }
}
