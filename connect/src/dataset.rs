// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Dataset identification, streaming and processing on the Ark.

use crate::{Error, schema};
use darkbio_wire::protocol::{Message, Promise, Requester};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::time::{Duration, Instant};

const PEEK_SIZE: usize = 1024 * 1024;
const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024; // Leave room for sealing and framing
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Upload stages reported on the caller's thread. Acknowledged bytes may still
/// need writing or validation; only successful processing completes the upload.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadProgress {
    /// Fetching the reference dataset advertised by the Ark.
    Downloading,
    /// Asking the Ark to identify the file from its first chunk.
    Identifying,
    /// The Ark's identification, including its summary and confidence.
    Identified(schema::SlotUploadPeekResponse),
    /// Opening an upload session. The Ark may request companion approval.
    Preparing,
    /// Dataset bytes acknowledged by the Ark so far.
    Uploading { uploaded: u64, total: u64 },
    /// Validation and indexing progress reported by the Ark.
    Processing(schema::SlotUploadProcessResponse),
}

/// A local file needs identification; a reference already names its target
/// slot and carries the hash advertised alongside the download.
pub(crate) struct Dataset {
    pub name: String,
    pub size: u64,
    pub reference: Option<(i32, [u8; 32])>, // Slot kind and expected content hash
}

impl Dataset {
    /// Takes the download offer from slot metadata without guessing builds or
    /// checking slot dependencies. The Ark decides whether an upload is allowed.
    pub(crate) fn reference(slot: &schema::SlotStatus) -> Result<(Self, String), Error> {
        use schema::slot_status::Meta;
        let (url, size, hash) = match &slot.meta {
            Some(Meta::ReferenceGenome(meta)) => (
                &meta.download_url,
                meta.download_bytes,
                &meta.download_sha256,
            ),
            Some(Meta::GeneAnnotations(meta)) => (
                &meta.download_url,
                meta.download_bytes,
                &meta.download_sha256,
            ),
            Some(Meta::VariantCatalog(meta)) => (
                &meta.download_url,
                meta.download_bytes,
                &meta.download_sha256,
            ),
            _ => {
                return Err(Error::Dataset(
                    "slot advertises no reference download".into(),
                ));
            }
        };
        if url.is_empty() || size == 0 {
            return Err(Error::Dataset(
                "slot advertises no reference download".into(),
            ));
        }
        let uri: ureq::http::Uri = url
            .parse()
            .map_err(|_| Error::Dataset("invalid reference download URL".into()))?;
        if uri.scheme_str() != Some("https")
            || uri.host().is_none_or(str::is_empty)
            || uri
                .authority()
                .is_some_and(|value| value.as_str().contains('@'))
        {
            return Err(Error::Dataset(
                "reference download requires an HTTPS URL without credentials".into(),
            ));
        }
        let name = uri
            .path()
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Dataset("reference download URL has no filename".into()))?;
        let mut sha256 = [0; 32];
        hex::decode_to_slice(hash, &mut sha256)
            .map_err(|_| Error::Dataset("invalid reference SHA-256".into()))?;
        Ok((
            Self {
                name: name.into(),
                size,
                reference: Some((slot.kind, sha256)),
            },
            url.clone(),
        ))
    }
}

/// Public reference hosts receive no cloud or package credentials. Redirects
/// remain HTTPS, and the download shares the upload's absolute deadline.
pub(crate) fn download(
    url: &str,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
        .http_status_as_error(false)
        .build()
        .into();
    fetch(&agent, url, deadline)
}

fn fetch(
    agent: &ureq::Agent,
    url: &str,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Error> {
    let response = agent
        .get(url)
        .header("Accept-Encoding", "identity")
        .config()
        .timeout_global(Some(remaining(deadline)?))
        .build()
        .call()
        .map_err(|error| match error {
            ureq::Error::Timeout(_) => Error::Timeout,
            error => Error::Dataset(format!("reference download failed: {error}")),
        })?;
    if response.status() != ureq::http::StatusCode::OK {
        return Err(Error::Dataset(format!(
            "reference download returned HTTP {}",
            response.status()
        )));
    }
    Ok(response)
}

/// Identifies once, resends that same head in the authorized start request and
/// streams the rest. A failed session is cancelled within the remaining deadline;
/// cleanup never replaces the original error or retries an upload.
pub(crate) fn upload(
    requester: &Requester,
    dataset: &Dataset,
    reader: &mut impl Read,
    deadline: Instant,
    mut progress: impl FnMut(UploadProgress),
) -> Result<(), Error> {
    if dataset.size == 0 {
        return Err(Error::Dataset("dataset is empty".into()));
    }
    let head = read_chunk(
        reader,
        dataset.size.min(PEEK_SIZE as u64) as usize,
        deadline,
    )?;
    let mut hash = dataset.reference.map(|_| Sha256::new());
    if let Some(hash) = &mut hash {
        hash.update(&head);
    }
    if head.len() as u64 == dataset.size {
        finish_read(reader, hash.take(), dataset, deadline)?;
    }
    let kind = match dataset.reference {
        Some((kind, _)) => kind,
        None => {
            progress(UploadProgress::Identifying);
            let identified = requester
                .request(
                    schema::SlotUploadPeekRequest {
                        name: dataset.name.clone(),
                        size: dataset.size,
                        chunk: head.clone(),
                        kinds: Vec::new(),
                    },
                    deadline,
                )?
                .wait::<schema::SlotUploadPeekResponse>()?;
            if !identified.reject.is_empty() {
                return Err(Error::Dataset(identified.reject));
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
            deadline,
        )?
        .wait::<schema::SlotUploadStartResponse>()?
        .session;
    let result = (|| {
        progress(UploadProgress::Uploading {
            uploaded,
            total: dataset.size,
        });
        let mut sent = uploaded;
        let mut pending: Option<(Promise<Message>, u64)> = None;
        while sent < dataset.size {
            let size = (dataset.size - sent).min(CHUNK_SIZE as u64) as usize;
            let chunk = read_chunk(reader, size, deadline)?;
            if let Some(hash) = &mut hash {
                hash.update(&chunk);
            }
            sent += size as u64;
            if sent == dataset.size {
                finish_read(reader, hash.take(), dataset, deadline)?;
            }
            // Keep at most two chunks outstanding so device writes can overlap
            // the next transfer. The last acknowledgement is awaited too.
            let next =
                requester.request(schema::SlotUploadChunkRequest { session, chunk }, deadline)?;
            if let Some((previous, bytes)) = pending.take() {
                previous.wait::<schema::SlotUploadChunkResponse>()?;
                uploaded += bytes;
                progress(UploadProgress::Uploading {
                    uploaded,
                    total: dataset.size,
                });
            }
            pending = Some((next, size as u64));
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
                .request(schema::SlotUploadProcessRequest { session }, deadline)?
                .wait::<schema::SlotUploadProcessResponse>()?;
            if !status.failure.is_empty() {
                return Err(Error::Dataset(status.failure));
            }
            // An empty or malformed report must not turn into false success.
            let phases = status.phase_names.len() as u64;
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
            std::thread::sleep(POLL_INTERVAL.min(remaining(deadline)?));
        }
    })();
    if result.is_err() {
        let cleanup = deadline.min(Instant::now() + Duration::from_secs(1));
        let _ = requester
            .request(schema::SlotUploadCancelRequest { session }, cleanup)
            .and_then(|pending| pending.wait::<schema::SlotUploadCancelResponse>());
    }
    result
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(Error::Timeout)
}

fn read_chunk(reader: &mut impl Read, size: usize, deadline: Instant) -> Result<Vec<u8>, Error> {
    remaining(deadline)?;
    let mut chunk = vec![0; size];
    reader.read_exact(&mut chunk).map_err(read_error)?;
    remaining(deadline)?;
    Ok(chunk)
}

/// Checks EOF and the advertised hash before sending the final chunk. Earlier
/// chunks may already be accepted, but a bad source never reaches processing.
fn finish_read(
    reader: &mut impl Read,
    hash: Option<Sha256>,
    dataset: &Dataset,
    deadline: Instant,
) -> Result<(), Error> {
    loop {
        remaining(deadline)?;
        match reader.read(&mut [0]) {
            Ok(0) => break,
            Ok(_) => return Err(Error::Dataset("dataset exceeds its advertised size".into())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_error(error)),
        }
    }
    remaining(deadline)?;
    if let (Some(hash), Some((_, expected))) = (hash, dataset.reference)
        && hash.finalize().as_slice() != expected
    {
        return Err(Error::Dataset(
            "reference SHA-256 does not match the advertised dataset".into(),
        ));
    }
    Ok(())
}

fn read_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut
        || matches!(
            error
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<ureq::Error>()),
            Some(ureq::Error::Timeout(_))
        )
    {
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
            phase_names: vec!["Validate".into(), "Index".into()],
            phase_in: phase,
            phase_progress: progress,
            ..Default::default()
        }
    }

    fn source(size: usize, reference: Option<[u8; 32]>) -> Dataset {
        Dataset {
            name: "sample.vcf.gz".into(),
            size: size as u64,
            reference: reference.map(|hash| (3, hash)),
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
        let peer = Peer::spawn(Box::new(move |_, request, responder| {
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
                Content::SlotUploadPeek(request) => {
                    assert_eq!(request.name, "sample.vcf.gz");
                    assert!(request.kinds.is_empty());
                    observed.head = request.chunk;
                    (
                        "peek",
                        schema::SlotUploadPeekResponse {
                            kind: 2,
                            summary: "Variant calls".into(),
                            reject: if fail == Some("identify") {
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
        for size in [17, PEEK_SIZE, PEEK_SIZE + 2 * CHUNK_SIZE + 29] {
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

    /// A successful transfer may still fail validation. Neither a refusal nor a
    /// malformed progress report can be mistaken for completed processing.
    #[test]
    fn test_failures() {
        for fail in ["identify", "peek", "start", "chunk", "process"] {
            let bytes = vec![42; PEEK_SIZE + 2 * CHUNK_SIZE + 1];
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
        for size in [17, PEEK_SIZE + CHUNK_SIZE + 17] {
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
                    assert_eq!(observed.stages.contains(&"cancel"), size > PEEK_SIZE);
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
        let bytes = vec![42; PEEK_SIZE + 3 * CHUNK_SIZE];
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
            PEEK_SIZE + 2 * CHUNK_SIZE
        );
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    /// Expiration between stages cannot start processing, even if all upload
    /// acknowledgements arrived. An already expired deadline performs no reads.
    #[test]
    fn test_deadline() {
        let (mut peer, observed) = peer(None, vec![]);
        let session = attach(&mut peer);
        let deadline = Instant::now() + Duration::from_millis(150);
        let result = upload(
            &session.requester(),
            &source(1, None),
            &mut [42].as_slice(),
            deadline,
            |stage| {
                if matches!(stage, UploadProgress::Uploading { .. }) {
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                }
            },
        );
        assert!(matches!(result, Err(Error::Timeout)));
        assert!(!observed.lock().unwrap().stages.contains(&"process"));
        struct Unread;
        impl Read for Unread {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("expired operation read its source")
            }
        }
        assert!(matches!(
            upload(
                &session.requester(),
                &source(1, None),
                &mut Unread,
                Instant::now(),
                |_| {}
            ),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn test_reference_metadata() {
        let meta = schema::SlotMetaReferenceGenome {
            download_url: "https://reference.example/genome.fa.gz".into(),
            download_bytes: 123,
            download_sha256: "ab".repeat(32),
            ..Default::default()
        };
        let slot = |meta| schema::SlotStatus {
            kind: 0,
            meta: Some(schema::slot_status::Meta::ReferenceGenome(meta)),
            ..Default::default()
        };
        let (dataset, url) = Dataset::reference(&slot(meta.clone())).unwrap();
        assert_eq!(dataset.name, "genome.fa.gz");
        assert_eq!(dataset.reference, Some((0, [0xab; 32])));
        assert_eq!(url, meta.download_url);
        for url in [
            "",
            "file:///tmp/genome",
            "http://reference.example/genome",
            "https://user:password@reference.example/genome",
            "https://reference.example/",
        ] {
            assert!(
                Dataset::reference(&slot(schema::SlotMetaReferenceGenome {
                    download_url: url.into(),
                    ..meta.clone()
                }))
                .is_err()
            );
        }
        assert!(
            Dataset::reference(&slot(schema::SlotMetaReferenceGenome {
                download_sha256: "bad".into(),
                ..meta.clone()
            }))
            .is_err()
        );
        assert!(
            Dataset::reference(&slot(schema::SlotMetaReferenceGenome {
                download_bytes: 0,
                ..meta
            }))
            .is_err()
        );
        assert!(Dataset::reference(&schema::SlotStatus::default()).is_err());
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
        assert!(matches!(
            read_error(ureq::Error::Timeout(ureq::Timeout::Global).into_io()),
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
                    "sample.vcf.gz",
                    1,
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

    /// A download can redirect before streaming. Error pages and unsolicited
    /// partial responses cannot become dataset content.
    #[test]
    fn test_download() {
        use crate::cloud::tests::{response, serve};
        let body = "reference dataset";
        let (url, requests) = serve(vec![
            (Duration::ZERO, "HTTP/1.1 302 Found\r\nLocation: /sample.vcf.gz\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()),
            (Duration::ZERO, response(200, body)),
            (Duration::ZERO, response(404, "missing")),
            (Duration::ZERO, response(206, "partial")),
        ]);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .build()
            .into();
        let deadline = Instant::now() + TIMEOUT;
        let mut downloaded = fetch(&agent, &url, deadline).unwrap();
        let (mut peer, observed) = peer(None, vec![]);
        let session = attach(&mut peer);
        upload(
            &session.requester(),
            &source(body.len(), Some(Sha256::digest(body).into())),
            &mut downloaded.body_mut().as_reader(),
            deadline,
            |_| {},
        )
        .unwrap();
        assert_eq!(observed.lock().unwrap().bytes, body.as_bytes());
        for _ in 0..2 {
            assert!(matches!(
                fetch(&agent, &url, deadline),
                Err(Error::Dataset(_))
            ));
        }
        for _ in 0..4 {
            let request = requests.recv_timeout(TIMEOUT).unwrap().to_lowercase();
            assert!(request.contains("accept-encoding: identity\r\n"));
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("dark-auth:"));
        }
        // The production download agent refuses HTTP even when the caller
        // bypasses the metadata validator, including after a redirect.
        assert!(download(&url, deadline).is_err());
    }

    /// An HTTP deadline remains a timeout instead of a generic download error.
    #[test]
    fn test_download_timeout() {
        use crate::cloud::tests::{response, serve};
        let (url, _requests) = serve(vec![(Duration::from_millis(250), response(200, "late"))]);
        let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
        assert!(matches!(
            fetch(&agent, &url, Instant::now() + Duration::from_millis(50)),
            Err(Error::Timeout)
        ));
    }
}
