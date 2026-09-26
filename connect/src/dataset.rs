// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dataset identification, streaming and processing on the Ark.

use crate::{Error, Timing, schema};
use darkbio_clock::Clock;
use darkbio_wire::protocol::{Message, Promise, Requester};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::time::Duration;

/// Prefix supplied to the Ark for file identification and upload preparation.
const IDENTIFY_SIZE: usize = 1024 * 1024;
/// Largest upload chunk, 32 KiB short of a 2 MiB frame to leave room for
/// sealing and framing.
const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024;
/// Interval after which a chunk goes out partial, keeping the Ark's upload
/// session alive while the source is slow.
///
/// It is checked between source reads, so it never interrupts a read that
/// blocks.
const CHUNK_INTERVAL: Duration = Duration::from_secs(1);
/// Delay between processing reports, independent of each response's deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Upload stages reported on the caller's thread.
///
/// Acknowledged bytes may still need writing or validation; only successful
/// processing completes the upload.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadProgress {
    /// Asking the Ark to identify the file from its first chunk.
    Identifying,
    /// The Ark's identification, including its summary and confidence.
    Identified(schema::SlotIdentifyResponse),
    /// Opening an upload session, for which the Ark may request companion
    /// approval.
    Preparing,
    /// The upload session is available for explicit cancellation.
    Started {
        /// Upload ID accepted by [`schema::SlotUploadCancelRequest`].
        session: u64,
    },
    /// Dataset bytes acknowledged by the Ark so far.
    Uploading {
        /// Bytes acknowledged so far, including the identification prefix.
        uploaded: u64,
        /// Declared source length in bytes.
        total: u64,
    },
    /// Validation and indexing progress reported by the Ark.
    Processing(schema::SlotUploadProcessResponse),
}

/// Source to upload, either identified by the Ark or naming its target slot.
///
/// A local file needs identification; a reference already names its target
/// slot and carries the hash advertised alongside the download.
#[derive(Clone, Debug)]
pub struct Dataset {
    /// Source filename sent to the Ark for identification and display.
    pub name: String,
    /// Exact source length in bytes; truncation and trailing bytes are errors.
    pub size: u64,
    /// Target slot, or `None` to let the Ark identify the file.
    pub slot: Option<i32>,
    /// Optional SHA-256 checked before processing.
    pub sha256: Option<[u8; 32]>,
}

/// Uploads a dataset, identifying it once, resending that head in the start
/// request and streaming the rest.
///
/// After a failure it requests the session's cancellation within the remaining
/// deadline, at most 1 s. Cancellation errors are ignored, so cleanup never
/// replaces the original error, and it never retries an upload. Deadlines are
/// measured on the clock of the requester's session.
pub(crate) fn upload(
    requester: &Requester,
    dataset: &Dataset,
    reader: &mut impl Read,
    timing: impl Into<Timing>,
    mut progress: impl FnMut(UploadProgress),
) -> Result<(), Error> {
    let clock = &requester.clock();
    let timing = timing.into();
    if dataset.size == 0 {
        return Err(Error::Dataset("dataset is empty".into()));
    }

    // Read and hash the identification head, checking a source ending there
    let head = read_chunk(
        reader,
        dataset.size.min(IDENTIFY_SIZE as u64) as usize,
        clock,
        timing,
        None,
    )?;
    let mut hash = dataset.sha256.map(|_| Sha256::new());
    if let Some(hash) = &mut hash {
        hash.update(&head);
    }
    if head.len() as u64 == dataset.size {
        finish_read(reader, hash.take(), dataset, clock, timing)?;
    }

    // Identify the dataset unless the caller named its slot
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
                    timing.io(clock),
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

    // Open the session with the same head, which may wait for approval
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
            timing.approval(clock),
        )?
        .wait::<schema::SlotUploadStartResponse>()?
        .session;

    // Run the session's steps as one outcome, so any failure can cancel it
    let result = (|| {
        progress(UploadProgress::Started { session });
        progress(UploadProgress::Uploading {
            uploaded,
            total: dataset.size,
        });

        // Stream the rest, checking the source ends at its declared size
        let mut sent = uploaded;
        let mut pending: Option<(Promise<Message>, u64)> = None;
        while sent < dataset.size {
            let size = (dataset.size - sent).min(CHUNK_SIZE as u64) as usize;
            let chunk = read_chunk(reader, size, clock, timing, Some(CHUNK_INTERVAL))?;
            let size = chunk.len() as u64;
            if let Some(hash) = &mut hash {
                hash.update(&chunk);
            }
            sent += size;
            if sent == dataset.size {
                finish_read(reader, hash.take(), dataset, clock, timing)?;
            }
            // Keep at most two chunks outstanding so device writes can overlap
            // the next transfer. The last acknowledgment is awaited too.
            let next = requester.request(
                schema::SlotUploadChunkRequest { session, chunk },
                timing.io(clock),
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

        // Poll processing until its last phase completes
        loop {
            let status = requester
                .request(
                    schema::SlotUploadProcessRequest { session },
                    timing.io(clock),
                )?
                .wait::<schema::SlotUploadProcessResponse>()?;
            if !status.failure.is_empty() {
                return Err(Error::Dataset(status.failure));
            }
            // An empty or malformed report must not turn into false success
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
            timing.pause(clock, POLL_INTERVAL)?;
        }
    })();

    // Request a failed session's cancellation within the remaining deadline, at
    // most 1 s, ignoring the outcome
    if result.is_err() {
        let cleanup = timing.io(clock).min(clock.now() + Duration::from_secs(1));
        let _ = requester
            .request(schema::SlotUploadCancelRequest { session }, cleanup)
            .and_then(|pending| pending.wait::<schema::SlotUploadCancelResponse>());
    }
    result
}

/// Reads up to `size` bytes, returning early once a read completes after
/// `interval` has passed since the call started.
///
/// The identification head is read without an interval, so it fills before a
/// session opens. Later chunks go out partial so a slow source keeps the Ark's
/// session alive. The interval is checked between reads and never interrupts
/// a blocking one. A source ending early is an error, and the caller's reader
/// still owns the timeout of each individual read.
fn read_chunk(
    reader: &mut impl Read,
    size: usize,
    clock: &Clock,
    timing: Timing,
    interval: Option<Duration>,
) -> Result<Vec<u8>, Error> {
    let start = clock.now();
    let mut chunk = vec![0; size];
    let mut filled = 0;
    while filled < size {
        timing.check(clock)?;
        match reader.read(&mut chunk[filled..]) {
            Ok(0) => return Err(read_error(io::ErrorKind::UnexpectedEof.into())),
            Ok(count) => filled += count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(read_error(err)),
        }
        if interval.is_some_and(|interval| clock.elapsed(start) >= interval) {
            break;
        }
    }
    timing.check(clock)?;
    chunk.truncate(filled);
    Ok(chunk)
}

/// Checks EOF and the advertised hash before sending the final chunk.
///
/// Earlier chunks may already be accepted, but a bad source never reaches
/// processing.
fn finish_read(
    reader: &mut impl Read,
    hash: Option<Sha256>,
    dataset: &Dataset,
    clock: &Clock,
    timing: Timing,
) -> Result<(), Error> {
    loop {
        timing.check(clock)?;
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
    timing.check(clock)?;
    if let (Some(hash), Some(expected)) = (hash, dataset.sha256)
        && hash.finalize().as_slice() != expected
    {
        return Err(Error::Integrity(
            "reference SHA-256 does not match the advertised dataset".into(),
        ));
    }
    Ok(())
}

/// Keeps source timeouts distinct from other dataset read failures.
fn read_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut {
        Error::Timeout
    } else {
        Error::DatasetRead(error)
    }
}

/// Dataset upload regressions against a scripted Ark.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrustMode;
    use crate::testing::{Peer, test_clock, wait_deadline};
    use darkbio_clock::TestClock;
    use darkbio_wire::protocol::{self, Session};
    use schema::host_to_ark::Content;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    /// Budget for test I/O that is not exercising expiration.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Wire exchange recorded by the scripted peer.
    #[derive(Default)]
    struct Observed {
        /// Stages the peer served, in arrival order.
        stages: Vec<&'static str>,
        /// Head the Ark was asked to identify.
        head: Vec<u8>,
        /// Dataset bytes the upload delivered, head included.
        bytes: Vec<u8>,
        /// Slot kind the upload session was opened for.
        kind: Option<i32>,
        /// Channel notifying a test each time an upload chunk reaches the Ark.
        chunks: Option<mpsc::Sender<()>>,
    }

    /// Builds a processing report of two phases, at `phase` with `progress` out
    /// of 10,000.
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

    /// Creates a dataset of `size` bytes, a reference into slot 3 when given
    /// its hash.
    fn source(size: usize, reference: Option<[u8; 32]>) -> Dataset {
        Dataset {
            name: "sample.vcf.gz".into(),
            size: size as u64,
            slot: reference.map(|_| 3),
            sha256: reference,
        }
    }

    /// Connects a raw wire session to the peer, pinning its identity key.
    fn attach(peer: &mut Peer) -> Session {
        let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
        protocol::connect(peer.stream(), &trust).unwrap().0
    }

    /// Spawns a peer recording the wire exchange, optionally failing a stage.
    ///
    /// Failing `identify` rejects the file in the identification answer, and
    /// any other stage name refuses that stage. A failing peer refuses the
    /// cancel too, which must never replace the failure that caused cleanup.
    /// Processing reports come from `reports`, then a finished one.
    fn peer(
        clock: &Clock,
        fail: Option<&'static str>,
        reports: Vec<schema::SlotUploadProcessResponse>,
    ) -> (Peer, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = observed.clone();
        let mut reports = VecDeque::from(reports);
        let peer = Peer::spawn(
            clock,
            Box::new(move |session, request, responder| {
                if matches!(request, Content::DeviceInfo(_)) {
                    return crate::testing::answering(session, request, responder);
                }
                let deadline = session.clock().now() + TIMEOUT;
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
                        if let Some(chunks) = &observed.chunks {
                            let _ = chunks.send(());
                        }
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
            }),
        );
        (peer, observed)
    }

    /// An upload resends its identified head, delivers every byte once, and
    /// polls on past an early phase at 100%.
    #[test]
    fn test_upload() {
        let mut tester = test_clock();
        let clock = tester.clock();
        for size in [17, IDENTIFY_SIZE, IDENTIFY_SIZE + 2 * CHUNK_SIZE + 29] {
            // Upload and process the source, the first report asking for
            // another poll
            let bytes: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let (mut peer, observed) =
                peer(&clock, None, vec![status(1, 10_000), status(2, 10_000)]);
            let session = attach(&mut peer);
            let deadline = clock.now() + TIMEOUT;
            let uploading = thread::spawn({
                let requester = session.requester();
                let bytes = bytes.clone();
                move || {
                    let mut progress = Vec::new();
                    let result = upload(
                        &requester,
                        &source(size, None),
                        &mut bytes.as_slice(),
                        deadline,
                        |stage| progress.push(stage),
                    );
                    result.map(|()| progress)
                }
            });

            // End the pause between the two reports once the uploader sleeps in it
            let poll = clock.now() + POLL_INTERVAL;
            wait_deadline(&tester, poll);
            tester.advance_to(poll);
            let progress = uploading.join().unwrap().unwrap();

            // Every byte arrived once, and processing took both reports
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

    /// A slow source sends a partial chunk before reading the rest.
    ///
    /// The reader continues only once the Ark receives that chunk.
    #[test]
    fn test_slow_source_flushes_partial_chunks() {
        /// Source whose second read takes a whole flush interval of the test
        /// clock, and whose third waits until the Ark received a chunk.
        struct Slow<'a> {
            /// Test clock the second read advances.
            tester: &'a mut TestClock,
            /// Bytes left to serve.
            bytes: &'a [u8],
            /// Count of reads served so far.
            reads: usize,
            /// Chunk arrivals at the Ark, awaited by the third read.
            chunks: mpsc::Receiver<()>,
        }
        impl Read for Slow<'_> {
            /// Serves the head, then 64 KiB reads, advancing the clock by one
            /// chunk interval on the second read and awaiting a chunk arrival
            /// on the third.
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads == 2 {
                    self.tester.advance(CHUNK_INTERVAL);
                } else if self.reads == 3 {
                    self.chunks.recv().unwrap();
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

        // Upload from the slow source through a peer reporting each chunk
        let mut tester = test_clock();
        let clock = tester.clock();
        let bytes = vec![42; IDENTIFY_SIZE + 128 * 1024];
        let (mut peer, observed) = peer(&clock, None, vec![]);
        let (arrived, chunks) = mpsc::channel();
        observed.lock().unwrap().chunks = Some(arrived);
        let session = attach(&mut peer);
        let mut reader = Slow {
            tester: &mut tester,
            bytes: &bytes,
            reads: 0,
            chunks,
        };
        upload(
            &session.requester(),
            &source(bytes.len(), None),
            &mut reader,
            clock.now() + TIMEOUT,
            |_| {},
        )
        .unwrap();

        // The rest arrives in two chunks, the first flushed early
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

    /// Neither a failed stage nor a failed or malformed processing report
    /// passes as a completed upload.
    #[test]
    fn test_failures() {
        // Each failed stage fails the upload and requests cancellation of any
        // session it opened
        let clock = test_clock().clock();
        for fail in ["identify", "peek", "start", "chunk", "process"] {
            let bytes = vec![42; IDENTIFY_SIZE + 2 * CHUNK_SIZE + 1];
            let (mut peer, observed) = peer(&clock, Some(fail), vec![]);
            let session = attach(&mut peer);
            let result = upload(
                &session.requester(),
                &source(bytes.len(), None),
                &mut bytes.as_slice(),
                clock.now() + TIMEOUT,
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

        // A failed or malformed processing report fails and cancels the upload
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
            let (mut peer, observed) = peer(&clock, None, vec![report]);
            let session = attach(&mut peer);
            let result = upload(
                &session.requester(),
                &source(1, None),
                &mut [42].as_slice(),
                clock.now() + TIMEOUT,
                |_| {},
            );
            assert!(matches!(result, Err(Error::Dataset(_))));
            assert_eq!(observed.lock().unwrap().stages.last(), Some(&"cancel"));
        }
    }

    /// References skip identification and keep their kind, while a bad length
    /// or hash stops processing in small and large files alike.
    #[test]
    fn test_integrity() {
        // Upload each size's original and damaged copies as references
        let clock = test_clock().clock();
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
                let (mut peer, observed) = peer(&clock, None, vec![]);
                let session = attach(&mut peer);
                let result = upload(
                    &session.requester(),
                    &source(size, Some(hash)),
                    &mut bytes.as_slice(),
                    clock.now() + TIMEOUT,
                    |_| {},
                );

                // References skip identification, and only the original is
                // processed, a large damaged copy requesting its open session's
                // cancellation
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

    /// The source is read no further than two outstanding chunks, even while
    /// their acknowledgments arrive reversed.
    #[test]
    fn test_transfer_window() {
        // The peer answers the second chunk first, holding the first chunk's
        // acknowledgment until the test releases it
        let clock = test_clock().clock();
        let (notice, notices) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut held = None;
        let mut chunks = 0;
        let mut peer = Peer::spawn(
            &clock,
            Box::new(move |session, request, responder| {
                let deadline = session.clock().now() + TIMEOUT;
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
                                released.recv().unwrap();
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
            }),
        );

        // Upload through a source counting what the uploader read
        let session = attach(&mut peer);
        let bytes = vec![42; IDENTIFY_SIZE + 3 * CHUNK_SIZE];
        let source = source(bytes.len(), Some(Sha256::digest(&bytes).into()));
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        /// Source counting the bytes read from it.
        struct Counting<'a> {
            /// Bytes left to serve.
            bytes: &'a [u8],
            /// Bytes read so far, shared with the test.
            count: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Read for Counting<'_> {
            /// Reads from the source and adds the byte count to the shared
            /// total.
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(buf)?;
                self.count
                    .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                Ok(count)
            }
        }
        let counted = read.clone();
        let requester = session.requester();
        let deadline = clock.now() + TIMEOUT;
        let worker = thread::spawn(move || {
            upload(
                &requester,
                &source,
                &mut Counting {
                    bytes: &bytes,
                    count: counted,
                },
                deadline,
                |_| {},
            )
        });

        // With two chunks outstanding, the source is read no further
        notices.recv().unwrap();
        assert_eq!(
            read.load(std::sync::atomic::Ordering::SeqCst),
            IDENTIFY_SIZE + 2 * CHUNK_SIZE
        );
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    /// Short reads and interrupted calls of a caller's reader still upload the
    /// whole source, and a timed out read is a timeout.
    #[test]
    fn test_reader_errors() {
        /// Source serving one byte per read, interrupting every other call.
        struct Fragmented {
            /// Read calls made so far.
            calls: usize,
            /// Bytes left to serve.
            bytes: &'static [u8],
        }
        impl Read for Fragmented {
            /// Interrupts every odd call and serves a single byte on the others.
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls % 2 == 1 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let size = buf.len().min(1);
                self.bytes.read(&mut buf[..size])
            }
        }

        // A fragmenting and interrupting reader still delivers the whole source
        let clock = test_clock().clock();
        let (mut peer, _) = peer(&clock, None, vec![]);
        let session = attach(&mut peer);
        let mut reader = Fragmented {
            calls: 0,
            bytes: b"hello",
        };
        upload(
            &session.requester(),
            &source(5, None),
            &mut reader,
            clock.now() + TIMEOUT,
            |_| {},
        )
        .unwrap();

        // A timed out read maps to a timeout
        assert!(matches!(
            read_error(io::ErrorKind::TimedOut.into()),
            Error::Timeout
        ));
    }

    /// Repeated uploads through a client sync with the cloud once, attaching no
    /// relay the Ark does not ask for.
    #[test]
    fn test_client_setup() {
        use crate::cloud::tests::{response, serve};

        // Upload twice through a client of a cloud-routed connection
        let clock = test_clock().clock();
        let (url, requests) = serve(vec![
            response(200, r#"{"signer":"AQ==","crypto":"Ag=="}"#),
            response(200, r#"{"unixmilli":123,"signature":"BA=="}"#),
        ]);
        let (mut peer, observed) = peer(&clock, None, vec![]);
        let ark = crate::cloud::tests::attach(&mut peer, url);
        for _ in 0..2 {
            ark.client()
                .upload_dataset(
                    &source(1, None),
                    &mut [42].as_slice(),
                    clock.now() + TIMEOUT,
                    |_| {},
                )
                .unwrap();
        }

        // Only the first upload synced, before identifying the dataset
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
        assert!(requests.recv().unwrap().contains("/cloudsync/identity"));
        assert!(
            requests
                .recv()
                .unwrap()
                .contains("/cloudsync/time?challenge=03")
        );
    }

    /// The processing operation may outlive one wait allowance, even when
    /// successive replies report the same progress percentage.
    #[test]
    fn test_processing_renews_waits() {
        // Upload with a machine allowance shorter than the pause between polls
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut peer, observed) = peer(
            &clock,
            None,
            vec![status(1, 100), status(1, 100), status(2, 10_000)],
        );
        let session = attach(&mut peer);
        let uploading = thread::spawn({
            let requester = session.requester();
            move || {
                upload(
                    &requester,
                    &source(1, None),
                    &mut [42].as_slice(),
                    Timing::inactivity(Duration::from_millis(250)),
                    |_| {},
                )
            }
        });

        // Each pause lasts a whole poll interval, each report renewing the allowance
        for _ in 0..2 {
            let poll = clock.now() + POLL_INTERVAL;
            wait_deadline(&tester, poll);
            tester.advance_to(poll);
        }
        uploading.join().unwrap().unwrap();
        assert!(!observed.lock().unwrap().stages.contains(&"cancel"));
    }
}
