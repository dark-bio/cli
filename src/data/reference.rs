// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Reference data downloads, with their planning, transport recovery and cache
//! replay.
//!
//! Each attempt starts a new Ark upload and replays the retained bytes from disk.
//! The HTTP range request opens only when replay reaches the missing suffix.
//! Opening it earlier leaves the response unread while a large prefix uploads.

use super::{Progress, cache, download, filled, select};
use crate::{
    args::slot_name,
    context::{Connection, Context},
    error::Error,
    http,
    output::Output,
};
use darkbio_clock::Clock;
use darkbio_connect::{Dataset, schema::SlotStatus};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Validates the whole reference plan before mutation, then installs in
/// dependency order.
///
/// Stops at the first failure and reports completed, failed and unattempted
/// slots. A dry run only plans.
pub(super) fn fetch(
    context: &Context,
    connection: &Connection,
    slots: &[SlotStatus],
    id: Option<i32>,
    dry_run: bool,
    directory: Option<&Path>,
    no_cache: bool,
) -> Result<(), Error> {
    // Order the plan and validate every offer it downloads before any change
    let directory = directory
        .map(Path::to_path_buf)
        .unwrap_or_else(cache::directory);
    let ordered = plan(slots, id)?;
    for slot in ordered.iter().filter(|slot| !filled(slot)) {
        offer(slot)?;
    }

    // Start each row as skipped, planned or not attempted, and keep them as
    // the partial result
    let mut rows:Vec<Value>=ordered.iter().map(|slot| {
        let offer=download(slot);
        json!({"slot":slot_name(slot.kind),"id":slot.kind,"url":offer.map(|o|o.0),"size_bytes":offer.map(|o|o.1),"sha256":offer.map(|o|o.2),
            "cached":!no_cache && offer.is_some_and(|o|cache::cached(&directory, o.2)),
            "outcome":if filled(slot) {"skipped"} else if dry_run {"planned"} else {"not-attempted"},"error":null})
    }).collect();
    context.interrupt.partial(json!({"fetched":rows}));

    // Install in order, skipping filled slots and stopping at the first failure
    let mut failure = None;
    for (index, slot) in ordered.iter().enumerate() {
        if filled(slot) {
            context.output.event(
                "note",
                format!("{} is already filled", slot_name(slot.kind)),
            );
            continue;
        }
        let result = offer(slot).and_then(|source| {
            if dry_run {
                Ok(())
            } else {
                context
                    .output
                    .title(&format!("Fetching {}", slot_name(slot.kind)));
                install(
                    context,
                    connection,
                    &source,
                    if no_cache { None } else { Some(&directory) },
                )
            }
        });
        match result {
            Ok(()) => {
                if !dry_run {
                    rows[index]["outcome"] = json!("done");
                }
            }
            Err(error) => {
                rows[index]["outcome"] = json!("failed");
                rows[index]["error"] = error.json();
                context.interrupt.partial(json!({"fetched":rows}));
                failure = Some(error);
                break;
            }
        }
        context.interrupt.partial(json!({"fetched":rows}));
    }

    // Print every row, then return the first failure
    context.output.table(
        &json!({"fetched":rows}),
        &rows,
        &[
            ("SLOT", "slot"),
            ("SIZE", "size_bytes"),
            ("CACHED", "cached"),
            ("OUTCOME", "outcome"),
        ],
    )?;
    failure.map_or(Ok(()), Err)
}

/// Orders the reference slots after their dependencies, or picks the one slot
/// `id` names.
///
/// Dependencies decide the order even for slot kinds this tool does not know.
/// A cycle, or an empty slot's dependency that is neither filled nor planned
/// before it, is refused.
fn plan(slots: &[SlotStatus], id: Option<i32>) -> Result<Vec<&SlotStatus>, Error> {
    // A named slot is fetched alone
    if let Some(id) = id {
        return Ok(vec![select(slots, id)?]);
    }

    // Take the first slot without a pending dependency, since none means a
    // cycle
    let mut pending: Vec<_> = slots
        .iter()
        .filter(|slot| slot.origin == darkbio_connect::schema::SlotOrigin::OriginReference as i32)
        .collect();
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let index = pending
            .iter()
            .position(|slot| {
                filled(slot)
                    || slot
                        .deps
                        .iter()
                        .all(|id| !pending.iter().any(|other| other.kind == *id))
            })
            .ok_or_else(|| {
                Error::new(
                    5,
                    "dependency-missing",
                    "reference dependencies form a cycle",
                )
            })?;
        let slot = pending.remove(index);

        // An empty slot's dependencies must be filled or planned before it
        for dependency in slot.deps.iter().filter(|_| !filled(slot)) {
            if !slots.iter().any(|other| {
                other.kind == *dependency
                    && (filled(other)
                        || ordered
                            .iter()
                            .any(|ordered: &&SlotStatus| ordered.kind == *dependency))
            }) {
                return Err(Error::new(
                    5,
                    "dependency-missing",
                    format!(
                        "{} requires {}",
                        slot_name(slot.kind),
                        slot_name(*dependency)
                    ),
                ));
            }
        }
        ordered.push(slot);
    }
    Ok(ordered)
}

/// Validated reference offer used by both protocol upload and HTTP/cache handling.
struct Source {
    /// Filename, exact length, target slot and binary digest passed to connect.
    dataset: Dataset,
    /// Advertised HTTPS download URL without embedded credentials.
    url: String,
    /// Canonical lowercase SHA-256 used as a cache basename.
    hash: String,
}

/// Validates a nonempty HTTPS offer and its complete digest before any
/// download or upload.
fn offer(slot: &SlotStatus) -> Result<Source, Error> {
    // The slot must advertise a download with a URL that parses
    let (url, size, hash) = download(slot).ok_or_else(|| {
        Error::new(
            5,
            "no-download",
            format!("{} advertises no download", slot_name(slot.kind)),
        )
    })?;
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| Error::new(1, "file-rejected", "invalid reference URL"))?;

    // Require HTTPS to a host, without credentials in the URL
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(Error::new(
            1,
            "file-rejected",
            "reference download requires HTTPS without credentials",
        ));
    }

    // Require a nonempty length, a complete digest and a filename
    if size == 0 {
        return Err(Error::new(
            1,
            "file-rejected",
            "reference download is empty",
        ));
    }
    let mut sha256 = [0; 32];
    hex::decode_to_slice(hash, &mut sha256)
        .map_err(|_| Error::new(1, "file-rejected", "invalid reference SHA-256"))?;
    let name = uri
        .path()
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::new(1, "file-rejected", "reference URL has no filename"))?;
    Ok(Source {
        dataset: Dataset {
            name: name.into(),
            size,
            slot: Some(slot.kind),
            sha256: Some(sha256),
        },
        url: url.into(),
        hash: hex::encode(sha256),
    })
}

/// Replays a complete cache or streams a download through a fresh Ark upload.
///
/// At most three network attempts resume retained bytes when possible. Protocol
/// refusals are returned; only source failures or a corrupt prefix permit replay.
/// A refused HTTP range discards the prefix before the next attempt.
fn install(
    context: &Context,
    connection: &Connection,
    source: &Source,
    directory: Option<&Path>,
) -> Result<(), Error> {
    let mut directory = directory;
    let clock = connection.client.clock();

    // Upload a complete cached copy first, removing one that is corrupt or
    // unreadable
    if let Some(directory) = directory {
        let path = directory.join(&source.hash);
        if let Ok(mut file) = File::open(&path) {
            let mut progress = Progress::new(
                context,
                clock.clone(),
                source.dataset.slot.expect("reference slot"),
            );
            let result = connection.client.upload_dataset(
                &source.dataset,
                &mut file,
                context.timing(),
                |stage| progress.update(stage),
            );
            context.interrupt.clear();
            match result {
                Ok(()) => {
                    context
                        .output
                        .event("note", format!("cache: {}", path.display()));
                    return Ok(());
                }
                Err(darkbio_connect::Error::Integrity(_)) => {
                    context.output.event(
                        "warning",
                        "cached reference failed verification; downloading it again",
                    );
                    let _ = fs::remove_file(&path);
                }
                Err(darkbio_connect::Error::DatasetRead(_)) => {
                    let _ = fs::remove_file(&path);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    // Download in up to three attempts, each locking the cache entry or
    // streaming without one when it is unavailable
    let agent = http::agent(Duration::from_secs(context.options.timeout), 5);
    for attempt in 0..3 {
        let entry = match directory {
            Some(path) => {
                match cache::Entry::open(path, &source.hash, &source.url, source.dataset.size) {
                    Ok(entry) => Some(entry),
                    Err(_) => {
                        context.output.event(
                            "warning",
                            "cache unavailable or in use; streaming without a retained copy",
                        );
                        directory = None;
                        None
                    }
                }
            }
            None => None,
        };

        // Upload through the reader, stamping each session start and
        // acknowledgement for its stall check
        let mut reader = Reader::new(&agent, source, &context.output, clock.clone(), entry)?;
        let last_upload = reader.last_upload.clone();
        let mut progress = Progress::new(
            context,
            clock.clone(),
            source.dataset.slot.expect("reference slot"),
        );
        let result = connection.client.upload_dataset(
            &source.dataset,
            &mut reader,
            context.timing(),
            |stage| {
                if matches!(
                    stage,
                    darkbio_connect::UploadProgress::Started { .. }
                        | darkbio_connect::UploadProgress::Uploading { .. }
                ) {
                    last_upload.set(Some(clock.now()));
                }
                progress.update(stage);
            },
        );
        context.interrupt.clear();

        // A verified download is reusable even if the Ark later rejects processing.
        // Cache completion describes the source, not the slot's resulting state.
        let valid = reader.read == source.dataset.size
            && reader.hash.clone().finalize().as_slice() == source.dataset.sha256.unwrap();
        let failed = reader.failed;
        let entry = if let Some(writer) = reader.writer.take() {
            let entry = writer.finish(valid);
            if entry.is_none() {
                directory = None;
            }
            entry
        } else {
            // Replay may finish or fail before any HTTP request starts. The
            // reader still owns the entry when no append worker was needed.
            reader.entry.take()
        };

        // Publish a verified copy, drop a full-length corrupt one, and keep a
        // shorter prefix for resuming
        if let Some(entry) = entry {
            if valid {
                if let Err(error) = cache::complete(entry, &source.hash) {
                    context.output.event(
                        "warning",
                        format!("could not retain complete cache entry: {error}"),
                    );
                }
            } else if reader.read == source.dataset.size {
                let path = entry.path.clone();
                drop(entry);
                let _ = fs::remove_file(&path);
            }
        }

        // Retry a source failure or a corrupt prefix while attempts remain, and
        // otherwise end with the outcome
        match result {
            Ok(()) => {
                if let Some(directory) = directory {
                    context.output.event(
                        "note",
                        format!("cache: {}", directory.join(&source.hash).display()),
                    );
                }
                return Ok(());
            }
            Err(darkbio_connect::Error::Integrity(_)) if reader.offset > 0 && attempt < 2 => {
                context.output.event(
                    "warning",
                    "cached prefix failed verification; restarting download",
                )
            }
            Err(_) if failed && attempt < 2 => context.output.event(
                "warning",
                format!("download interrupted; retry {}/3", attempt + 2),
            ),
            Err(error) if failed => {
                return Err(reader.error.unwrap_or_else(|| match error {
                    darkbio_connect::Error::Timeout => error.into(),
                    darkbio_connect::Error::DatasetRead(error) => http::read_error(error),
                    error => error.into(),
                }));
            }
            Err(error) => return Err(reader.error.unwrap_or_else(|| error.into())),
        }
    }
    unreachable!("last attempt returns its result")
}

/// Checks that a partial response covers exactly the bytes from `offset` to
/// the end of the advertised `size`.
fn valid_range(response: &ureq::http::Response<ureq::Body>, offset: u64, size: u64) -> bool {
    if response.status() != 206 {
        return false;
    }
    let Some(range) = response
        .headers()
        .get("Content-Range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
    else {
        return false;
    };
    let Some((span, total)) = range.split_once('/') else {
        return false;
    };
    let Some((start, end)) = span.split_once('-') else {
        return false;
    };
    start.parse::<u64>() == Ok(offset)
        && total.parse::<u64>() == Ok(size)
        && size
            .checked_sub(1)
            .is_some_and(|last| end.parse::<u64>() == Ok(last))
}

/// Source reader that replays a retained prefix, then tees network bytes into a
/// best-effort cache.
///
/// It hashes both sources together and marks source failures for the download
/// retry policy. The cache lock stays held across replay, response validation
/// and queued writes.
struct Reader<'a> {
    /// Connection clock that measures the upload window.
    clock: Clock,
    /// HTTP client whose first request waits until the prefix has been read.
    agent: &'a ureq::Agent,
    /// Advertised URL, length and digest for this attempt.
    source: &'a Source,
    /// Warning sink for best-effort cache persistence.
    output: &'a Output,
    /// Locked cache entry, moved to the writer once the response is validated.
    entry: Option<cache::Entry>,
    /// Retained byte count used for the range request after replay.
    offset: u64,
    /// Session start or latest upload acknowledgement, shared with progress
    /// callbacks.
    last_upload: Rc<Cell<Option<Instant>>>,
    /// Retained prefix with an independent cursor capped at the resume offset.
    prefix: Option<io::Take<File>>,
    /// HTTP body opened only when the reader reaches the network suffix.
    network: Option<ureq::BodyReader<'static>>,
    /// Optional append worker receiving only newly downloaded bytes.
    writer: Option<cache::Writer>,
    /// Digest of all bytes replayed or downloaded in this attempt.
    hash: Sha256,
    /// Combined prefix and network byte count consumed by connect.
    read: u64,
    /// Whether a source failure permits another download attempt.
    failed: bool,
    /// Request or cache setup error preserved through connect's reader boundary.
    error: Option<Error>,
}

impl<'a> Reader<'a> {
    /// Opens only the cached prefix, since a live response must not wait
    /// through replay.
    ///
    /// It retains the cache lock even if the reader never reaches the network.
    fn new(
        agent: &'a ureq::Agent,
        source: &'a Source,
        output: &'a Output,
        clock: Clock,
        entry: Option<cache::Entry>,
    ) -> io::Result<Self> {
        let offset = entry
            .as_ref()
            .map(|entry| entry.len())
            .transpose()?
            .unwrap_or(0);
        let prefix = entry
            .as_ref()
            .filter(|_| offset > 0)
            .map(|entry| entry.prefix().map(|file| file.take(offset)))
            .transpose()?;
        Ok(Self {
            clock,
            agent,
            source,
            output,
            entry,
            offset,
            last_upload: Rc::new(Cell::new(None)),
            prefix,
            network: None,
            writer: None,
            hash: Sha256::new(),
            read: 0,
            failed: false,
            error: None,
        })
    }

    /// Requests the network source and validates the response before accepting
    /// any bytes or starting the cache writer.
    ///
    /// A refused range resets the cache and fails this attempt. Request failures
    /// permit retry; HTTP status failures on a full download are returned directly.
    fn open(&mut self) -> Result<(), Error> {
        // Ask for the whole file, or for the bytes past a retained prefix under
        // the cached validator
        let mut request = self
            .agent
            .get(&self.source.url)
            .header("Accept-Encoding", "identity");
        if self.offset > 0 {
            request = request.header("Range", format!("bytes={}-", self.offset));
            if let Some(validator) = self
                .entry
                .as_ref()
                .and_then(|entry| entry.meta.validator.as_ref())
            {
                request = request.header("If-Range", validator);
            }
        }

        // A failed request is a source failure, which permits another attempt
        let response = request.call().map_err(|error| {
            self.failed = true;
            http::error(error)
        })?;

        // A resume needs an exact range answer, and a full download a plain
        // success
        if self.offset > 0 && !valid_range(&response, self.offset, self.source.dataset.size) {
            // The Ark already has the prefix. Reset the cache and let connect
            // cancel this upload before the next attempt starts from zero.
            if let Some(entry) = &mut self.entry {
                entry.reset()?;
            }
            self.failed = true;
            return Err(Error::new(
                4,
                "cloud-unreachable",
                "server did not accept the download resume",
            ));
        }
        if self.offset == 0 && response.status() != 200 {
            return Err(Error::new(
                4,
                "cloud-unreachable",
                format!("reference download returned HTTP {}", response.status()),
            ));
        }

        // Keep the new validator for a later resume, and start the cache writer
        if let Some(entry) = &mut self.entry {
            entry.meta.validator = response
                .headers()
                .get("ETag")
                .or_else(|| response.headers().get("Last-Modified"))
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let Err(error) = entry.save() {
                self.output
                    .event("warning", format!("cannot save resume metadata: {error}"));
            }
        }
        self.writer = self
            .entry
            .take()
            .map(|entry| entry.writer(self.output.clone()))
            .transpose()?;
        self.network = Some(response.into_body().into_reader());
        Ok(())
    }
}

impl Read for Reader<'_> {
    /// Serves prefix bytes first, then records and caches network bytes.
    ///
    /// After a network read, it detects an upload window already lost to a
    /// source stall.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        // Replay the retained prefix first
        if let Some(prefix) = &mut self.prefix {
            let count = prefix.read(buffer)?;
            if count > 0 {
                self.hash.update(&buffer[..count]);
                self.read += count as u64;
                return Ok(count);
            }
            self.prefix = None;
        }

        // Open the network source once the prefix runs out
        if self.network.is_none() {
            // A complete partial file may have survived interruption just before
            // publication. Verify it without requesting a range beyond EOF.
            if self.read == self.source.dataset.size {
                return Ok(0);
            }
            if let Err(error) = self.open() {
                // Connect sees a source failure. Keep the original CLI error so
                // HTTP and cache setup failures retain their exit classification.
                let failure = io::Error::other(error.to_string());
                self.error = Some(error);
                return Err(failure);
            }
        }

        // Read network bytes, failing on a read error or an early end
        let count = match self.network.as_mut().expect("opened response").read(buffer) {
            Ok(count) => count,
            Err(error) => {
                self.failed = true;
                return Err(http::normalize_read_error(error));
            }
        };
        if count == 0 && self.read < self.source.dataset.size {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "reference download ended early",
            ));
        }

        // Cache, hash and count the bytes
        if let Some(writer) = &mut self.writer {
            writer.append(&buffer[..count]);
        }
        self.hash.update(&buffer[..count]);
        self.read += count as u64;

        // Fail a download that stalled past the upload window since the last
        // acknowledgement
        if self
            .last_upload
            .get()
            .is_some_and(|last| self.clock.elapsed(last) >= Duration::from_secs(30))
        {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "download outlived the Ark upload session",
            ));
        }
        Ok(count)
    }
}

/// Tests of reference planning, offer validation, resume and cache replay.
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use darkbio_clock::TestClock;
    use darkbio_connect::schema::{SlotDownload, SlotOrigin, SlotState};
    use std::io::Write;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    /// Counter that keeps parallel fixtures in separate temporary directories.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Temporary cache directory, removed with its files on drop.
    struct Directory(PathBuf);

    impl Directory {
        /// Creates an empty cache outside the user's real cache directory.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ark-reference-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Builds a quiet output, keeping expected cache diagnostics out of the
    /// test output.
    fn output() -> Output {
        Output::new(&crate::args::Cli::parse_from(["ark", "--quiet"]).options)
    }

    /// Builds a client that allows loopback HTTP while retaining production
    /// status handling.
    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into()
    }

    /// Builds a reference offer with the exact expected length and digest.
    fn source(url: String, bytes: &[u8]) -> Source {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        Source {
            dataset: Dataset {
                name: "reference.gz".into(),
                size: bytes.len() as u64,
                slot: Some(4),
                sha256: Some(hash),
            },
            url,
            hash: hex::encode(hash),
        }
    }

    /// Leaves only committed chunks and their validator for a fresh reader.
    fn interrupted(directory: &Path, source: &Source, bytes: &[u8]) {
        let mut entry =
            cache::Entry::open(directory, &source.hash, &source.url, source.dataset.size).unwrap();
        entry.meta.validator = Some("\"reference-v1\"".into());
        entry.save().unwrap();
        let mut writer = entry.writer(output()).unwrap();
        writer.append(bytes);
        drop(writer.finish(false).unwrap());
    }

    /// Serves scripted responses and records headers for resume assertions.
    fn serve(
        listener: TcpListener,
        replies: Vec<(u16, String, Vec<u8>)>,
    ) -> thread::JoinHandle<Vec<String>> {
        listener.set_nonblocking(false).unwrap();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, headers, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nETag: \"reference-v1\"\r\nConnection: close\r\n{headers}\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
                requests.push(String::from_utf8(request).unwrap().to_ascii_lowercase());
            }
            requests
        })
    }

    /// Builds a reference slot of kind `id` with its dependencies, filled or
    /// empty.
    fn reference(id: i32, deps: &[i32], filled: bool) -> SlotStatus {
        SlotStatus {
            kind: id,
            origin: SlotOrigin::OriginReference as i32,
            deps: deps.into(),
            state: if filled {
                SlotState::StateFilled
            } else {
                SlotState::StateEmpty
            } as i32,
            ..Default::default()
        }
    }

    /// The plan keeps filled slots in dependency order, and refuses cycles and
    /// missing dependencies of empty slots.
    #[test]
    fn dependency_order_retains_filled_slots_and_detects_missing_inputs() {
        let slots = [
            reference(3, &[1], false),
            reference(1, &[0], false),
            reference(0, &[], true),
        ];
        assert_eq!(
            plan(&slots, None)
                .unwrap()
                .iter()
                .map(|slot| slot.kind)
                .collect::<Vec<_>>(),
            [0, 1, 3]
        );
        assert!(
            plan(
                &[reference(1, &[2], false), reference(2, &[1], false)],
                None
            )
            .is_err()
        );
        assert!(plan(&[reference(1, &[2], false)], None).is_err());
        assert_eq!(plan(&[reference(1, &[2], true)], None).unwrap().len(), 1);
    }

    /// Offers need HTTPS without credentials, a filename and a complete digest,
    /// which is normalized to lowercase.
    #[test]
    fn offers_require_public_https_and_a_complete_hash() {
        for url in [
            "http://example.com/file",
            "https://user:secret@example.com/file",
            "https://example.com/",
        ] {
            let slot = SlotStatus {
                download: Some(SlotDownload {
                    url: url.into(),
                    bytes: 10,
                    sha256: "ab".repeat(32),
                }),
                ..reference(0, &[], false)
            };
            assert!(offer(&slot).is_err(), "{url}");
        }
        let mut slot = SlotStatus {
            download: Some(SlotDownload {
                url: "https://example.com/data.gz".into(),
                bytes: 10,
                sha256: "AB".repeat(32),
            }),
            ..reference(0, &[], false)
        };
        assert_eq!(offer(&slot).unwrap().hash, "ab".repeat(32));
        if let Some(download) = &mut slot.download {
            download.sha256 = "../outside".into();
        }
        assert!(offer(&slot).is_err());
        assert_eq!(
            offer(&reference(0, &[], false)).err().unwrap().code,
            "no-download"
        );
    }

    /// A body that ends early is a source failure, which permits a retry.
    #[test]
    fn truncated_sources_are_transport_failures() {
        let agent = http::agent(Duration::from_secs(1), 0);
        let output = output();
        let source = source("https://example.com/reference.gz".into(), &[1, 2, 3]);
        let clock = TestClock::new().clock();
        let mut reader = Reader::new(&agent, &source, &output, clock, None).unwrap();
        reader.network = Some(ureq::Body::builder().data([1, 2]).into_reader());
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(reader.failed);
        assert_eq!(reader.read, 2);
    }

    /// A resume accepts only a 206 covering exactly the remaining bytes.
    #[test]
    fn resume_requires_an_exact_range_response() {
        for (status, range, valid) in [
            (206, "bytes 1048576-2097160/2097161", true),
            (200, "bytes 1048576-2097160/2097161", false),
            (206, "bytes 0-2097160/2097161", false),
            (206, "bytes 1048576-2097159/2097161", false),
            (206, "bytes 1048576-2097160/3000000", false),
            (206, "bytes 1048576-2097160/*", false),
            (206, "garbage", false),
        ] {
            let response = ureq::http::Response::builder()
                .status(status)
                .header("Content-Range", range)
                .body(ureq::Body::builder().data([]))
                .unwrap();
            assert_eq!(
                valid_range(&response, 1048576, 2097161),
                valid,
                "{status} {range}"
            );
        }
    }

    /// A download read fails once the upload window has passed since the last
    /// acknowledgement.
    #[test]
    fn download_stall_tracks_the_upload_window() {
        // Read a two-byte body on the test clock
        let agent = http::agent(Duration::from_secs(1), 0);
        let output = output();
        let source = source("https://example.com/reference.gz".into(), &[42, 43]);
        let mut tester = TestClock::new();
        let mut reader = Reader::new(&agent, &source, &output, tester.clock(), None).unwrap();
        reader.network = Some(ureq::Body::builder().data([42, 43]).into_reader());

        // A read just inside the upload window passes
        reader.last_upload.set(Some(tester.clock().now()));
        tester.advance(Duration::from_secs(29));
        assert_eq!(reader.read(&mut [0]).unwrap(), 1);
        assert!(!reader.failed);

        // A read at the end of the window fails the download
        tester.advance(Duration::from_secs(1));
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(reader.failed);
    }

    /// A resumed download replays the cache before it opens the HTTP range
    /// request.
    #[test]
    fn restarted_download_opens_http_only_after_cache_replay() {
        // Download one chunk and a short tail from a loopback listener
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let bytes = [vec![42; cache::CHUNK], b"remaining bytes".to_vec()].concat();
        let source = source(
            format!("http://{}/reference.gz", listener.local_addr().unwrap()),
            &bytes,
        );
        let directory = Directory::new();

        // Keep one committed chunk and discard an interrupted tail. The next
        // reader has only the files left by the previous invocation.
        interrupted(&directory.0, &source, &bytes[..cache::CHUNK + 3]);
        let entry =
            cache::Entry::open(&directory.0, &source.hash, &source.url, source.dataset.size)
                .unwrap();
        let agent = agent();
        let output = output();
        let clock = TestClock::new().clock();
        let mut reader = Reader::new(&agent, &source, &output, clock, Some(entry)).unwrap();

        // The prefix replays without any HTTP request
        let mut received = vec![0; cache::CHUNK];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(received, bytes[..cache::CHUNK]);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        // The rest arrives through a range request, and the complete copy is
        // published
        let server = serve(
            listener,
            vec![(
                206,
                format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    cache::CHUNK,
                    bytes.len() - 1,
                    bytes.len()
                ),
                bytes[cache::CHUNK..].to_vec(),
            )],
        );
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, bytes);
        assert_eq!(
            reader.hash.clone().finalize().as_slice(),
            source.dataset.sha256.unwrap()
        );
        assert!(!reader.failed);
        cache::complete(
            reader.writer.take().unwrap().finish(true).unwrap(),
            &source.hash,
        )
        .unwrap();
        assert_eq!(fs::read(directory.0.join(&source.hash)).unwrap(), bytes);

        // The one request asks for the suffix under the cached validator
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(&format!("\r\nrange: bytes={}-\r\n", cache::CHUNK)));
        assert!(requests[0].contains("\r\nif-range: \"reference-v1\"\r\n"));
    }

    /// A refused resume discards the prefix, and the retry downloads from the
    /// start.
    #[test]
    fn refused_resume_discards_the_prefix_before_retry() {
        for status in [200, 206, 416] {
            // Keep one chunk, and script a refused resume and then the whole
            // file
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let bytes = [vec![42; cache::CHUNK], b"remaining bytes".to_vec()].concat();
            let source = source(
                format!("http://{}/reference.gz", listener.local_addr().unwrap()),
                &bytes,
            );
            let directory = Directory::new();
            interrupted(&directory.0, &source, &bytes[..cache::CHUNK]);
            let server = serve(
                listener,
                vec![
                    (
                        status,
                        "Content-Range: bytes 0-4/5\r\n".into(),
                        b"wrong".to_vec(),
                    ),
                    (200, String::new(), bytes.clone()),
                ],
            );
            let agent = agent();
            let output = output();
            let clock = TestClock::new().clock();

            // The refused resume fails the first attempt after the prefix replays
            {
                let entry = cache::Entry::open(
                    &directory.0,
                    &source.hash,
                    &source.url,
                    source.dataset.size,
                )
                .unwrap();
                let mut reader =
                    Reader::new(&agent, &source, &output, clock.clone(), Some(entry)).unwrap();
                let mut received = Vec::new();
                assert!(reader.read_to_end(&mut received).is_err());
                assert_eq!(received, bytes[..cache::CHUNK]);
                assert!(reader.failed);
                assert!(reader.writer.is_none());
            }

            // The prefix is gone, so the retry downloads everything without a
            // range
            let entry =
                cache::Entry::open(&directory.0, &source.hash, &source.url, source.dataset.size)
                    .unwrap();
            assert_eq!(entry.len().unwrap(), 0);
            let mut reader = Reader::new(&agent, &source, &output, clock, Some(entry)).unwrap();
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            assert_eq!(received, bytes);
            assert_eq!(
                reader.hash.finalize().as_slice(),
                source.dataset.sha256.unwrap()
            );
            cache::complete(
                reader.writer.take().unwrap().finish(true).unwrap(),
                &source.hash,
            )
            .unwrap();
            assert_eq!(fs::read(directory.0.join(&source.hash)).unwrap(), bytes);
            let requests = server.join().unwrap();
            assert!(requests[0].contains("\r\nrange:"));
            assert!(!requests[1].contains("\r\nrange:"));
            assert!(!requests[1].contains("\r\nif-range:"));
        }
    }

    /// A partial file that already holds every byte completes without an HTTP
    /// request.
    #[test]
    fn complete_partial_file_needs_no_http_request() {
        let bytes = vec![42; cache::CHUNK];
        let source = source("http://127.0.0.1:1/reference.gz".into(), &bytes);
        let directory = Directory::new();
        interrupted(&directory.0, &source, &bytes);
        let entry =
            cache::Entry::open(&directory.0, &source.hash, &source.url, source.dataset.size)
                .unwrap();
        let agent = agent();
        let output = output();
        let clock = TestClock::new().clock();
        let mut reader = Reader::new(&agent, &source, &output, clock, Some(entry)).unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, bytes);
        assert_eq!(
            reader.hash.finalize().as_slice(),
            source.dataset.sha256.unwrap()
        );
        assert!(reader.network.is_none());
        cache::complete(reader.entry.take().unwrap(), &source.hash).unwrap();
        assert_eq!(fs::read(directory.0.join(&source.hash)).unwrap(), bytes);
    }
}
