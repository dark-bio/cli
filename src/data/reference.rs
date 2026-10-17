// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Public data downloads, with their planning, transport recovery and cache
//! replay.
//!
//! A personal slot's download is verified before its one upload asks for approval.
//! Each reference attempt starts a new upload and replays retained bytes from disk.
//! The HTTP range request opens only when replay reaches the missing suffix.
//! Opening it earlier leaves the response unread while a large prefix uploads.

mod personal;

use super::{Operation, Progress, cache, download, filled, samples::Sample, select};
use crate::{
    args::slot_name,
    context::{Connection, Context},
    error::Error,
    http,
    output::Output,
};
use darkbio_clock::Clock;
use darkbio_connect::{
    Client, Dataset,
    schema::{SlotKind, SlotOrigin, SlotStatus},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Which slots a fetch fills, from the Ark's offers or from a sample.
pub(super) enum Target {
    /// Every reference slot the Ark offers a download for, in dependency order.
    All,
    /// One slot, from the download the Ark offers for it.
    Slot(i32),
    /// The snp-indel-calls slot, from a sample's pinned variant calls.
    Sample(Sample),
}

/// Plans the target's downloads and installs each one.
///
/// Planning all references also checks their dependencies and orders the work,
/// skipping filled slots and those without offers. A named slot is installed
/// alone, from the Ark's offer or a sample's pin. It stops at the first failure
/// and reports completed, failed and unattempted slots. A dry run only plans.
pub(super) fn fetch(
    context: &Context,
    connection: &Connection,
    slots: &[SlotStatus],
    target: Target,
    dry_run: bool,
    directory: Option<&Path>,
    no_cache: bool,
) -> Result<(), Error> {
    // A sample's pin stands in for an offer on the slot the Ark reports
    let (id, sample) = match target {
        Target::All => (None, None),
        Target::Slot(id) => (Some(id), None),
        Target::Sample(sample) => (Some(SlotKind::SnpIndelCalls as i32), Some(sample)),
    };
    let selected;
    let slots = if let Some(sample) = sample {
        selected = [SlotStatus {
            download: Some(sample.download()),
            ..select(slots, SlotKind::SnpIndelCalls as i32)?.clone()
        }];
        &selected
    } else {
        slots
    };

    // Order the plan and validate every offer it downloads before any change
    let directory = directory
        .map(Path::to_path_buf)
        .unwrap_or_else(cache::directory);
    let directory = if no_cache {
        None
    } else {
        Some(directory.as_path())
    };
    let ordered = plan(slots, id)?;
    let agent = http::agent(Duration::from_secs(context.options.timeout), 5);
    execute(context, &ordered, sample, dry_run, directory, |source| {
        install(context, &connection.client, source, directory, &agent)
    })
}

/// Installs a validated plan in order, retaining skipped and unattempted rows.
///
/// A dry run reports the plan without invoking the installer.
fn execute(
    context: &Context,
    ordered: &[&SlotStatus],
    sample: Option<Sample>,
    dry_run: bool,
    directory: Option<&Path>,
    mut install: impl FnMut(&Source) -> Result<(), Error>,
) -> Result<(), Error> {
    // Start each row as skipped, planned or not attempted, and keep them as
    // the partial result
    let mut rows: Vec<Value> = ordered
        .iter()
        .map(|slot| {
            let offer = download(slot);
            let mut row = json!({
                "slot": slot_name(slot.kind),
                "id": slot.kind,
                "url": offer.map(|offer| offer.0),
                "size_bytes": offer.map(|offer| offer.1),
                "sha256": offer.map(|offer| offer.2),
                "cached": directory.is_some_and(|directory| {
                    offer.is_some_and(|offer| cache::cached(directory, offer.2))
                }),
                "outcome": if filled(slot) || offer.is_none() {
                    "skipped"
                } else if dry_run {
                    "planned"
                } else {
                    "not-attempted"
                },
                "error": null,
            });
            if let Some(sample) = sample {
                row["sample"] = json!(sample.id());
                row["name"] = json!(sample.name());
            }
            row
        })
        .collect();
    context.interrupt.partial(json!({"fetched":rows}));

    // Install available offers in order, stopping at the first failure
    let mut failure = None;
    for (index, slot) in ordered.iter().enumerate() {
        if filled(slot) {
            context.output.event(
                "note",
                format!("{} is already filled", slot_name(slot.kind)),
            );
            continue;
        }
        if download(slot).is_none() {
            context.output.event(
                "note",
                format!("{} offers no download", slot_name(slot.kind)),
            );
            continue;
        }
        let result = offer(slot).and_then(|source| if dry_run { Ok(()) } else { install(&source) });
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
        Some("No reference datasets to fetch"),
    )?;
    failure.map_or(Ok(()), Err)
}

/// Orders the reference slots after their dependencies, or picks the one slot
/// `id` names, validating every offer the plan will install.
///
/// Dependency checks apply only when planning all references, where they
/// decide the order even for slot kinds this tool does not know. There a
/// cycle, or an empty slot's dependency that is neither filled nor offered
/// for installation before it, is refused. A slot `id` names is returned
/// without checking its dependencies, but needs an offer unless filled.
fn plan(slots: &[SlotStatus], id: Option<i32>) -> Result<Vec<&SlotStatus>, Error> {
    // A named slot is fetched alone
    if let Some(id) = id {
        let slot = select(slots, id)?;
        if !filled(slot) {
            offer(slot)?;
        }
        return Ok(vec![slot]);
    }

    // Take the first slot without a pending dependency, since none means a
    // cycle
    let mut pending: Vec<_> = slots
        .iter()
        .filter(|slot| slot.origin == darkbio_connect::schema::SlotOrigin::Reference as i32)
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

        // An empty slot's dependencies must be filled or offered before it
        for dependency in slot.deps.iter().filter(|_| !filled(slot)) {
            if !slots.iter().any(|other| {
                other.kind == *dependency
                    && (filled(other)
                        || (download(other).is_some()
                            && ordered
                                .iter()
                                .any(|ordered: &&SlotStatus| ordered.kind == *dependency)))
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

    // Missing offers are skipped, but malformed offers fail before any change
    for slot in ordered
        .iter()
        .filter(|slot| !filled(slot) && download(slot).is_some())
    {
        offer(slot)?;
    }
    Ok(ordered)
}

/// Validated public download, shared by the upload and the HTTP and cache code.
struct Source {
    /// Filename, exact length, target slot and binary digest passed to connect.
    dataset: Dataset,
    /// Advertised HTTPS download URL without embedded credentials.
    url: String,
    /// Canonical lowercase SHA-256 used as a cache basename.
    hash: String,
    /// Whether the target is a personal slot, whose upload the owner approves.
    personal: bool,
}

/// Validates a nonempty HTTPS offer and its complete digest before any
/// download or upload.
fn offer(slot: &SlotStatus) -> Result<Source, Error> {
    // The slot must advertise a download with a URL that parses
    let (url, size, hash) = download(slot).ok_or_else(|| {
        let error = Error::new(
            5,
            "no-download",
            format!("{} advertises no download", slot_name(slot.kind)),
        );
        if slot.kind == SlotKind::SnpIndelCalls as i32 {
            error.hint("--sample fills it with a public genome from `ark data samples`")
        } else {
            error
        }
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
        personal: slot.origin == SlotOrigin::Personal as i32,
    })
}

/// Installs a download into its slot.
///
/// A personal slot downloads and verifies the file before one upload, since
/// that upload asks the owner. A reference replays a cached copy or streams the
/// download into the Ark, in up to three attempts that resume kept bytes. Only
/// a broken source or a corrupt kept prefix earns another attempt, and a
/// refused range request drops the prefix first.
fn install(
    context: &Context,
    client: &Client,
    source: &Source,
    directory: Option<&Path>,
    agent: &ureq::Agent,
) -> Result<(), Error> {
    if source.personal {
        return personal::install(context, client, source, directory, agent);
    }
    let mut directory = directory;
    let clock = client.clock();
    let slot = source.dataset.slot.expect("download target slot");

    // Upload a complete cached copy first, removing one that is corrupt or
    // unreadable
    if let Some(directory) = directory {
        let path = directory.join(&source.hash);
        if let Ok(mut file) = File::open(&path) {
            let result = replay(context, client, source, &mut file);
            match result {
                Ok(()) => {
                    context
                        .output
                        .event("note", format!("cache: {}", path.display()));
                    return Ok(());
                }
                Err(darkbio_connect::Error::Integrity(_)) => {
                    context.output.fail_stage(None);
                    context.output.event(
                        "warning",
                        "cached download failed verification; downloading it again",
                    );
                    let _ = fs::remove_file(&path);
                }
                Err(darkbio_connect::Error::DatasetRead(_)) => {
                    context.output.fail_stage(None);
                    let _ = fs::remove_file(&path);
                }
                Err(error) => {
                    let error = Error::from(error);
                    context.output.fail_stage(Some(&error));
                    return Err(error);
                }
            }
        }
    }

    // Download in up to three attempts, each locking the cache entry or
    // streaming without one when it is unavailable
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
        // acknowledgment for its stall check
        let mut reader = Reader::new(agent, source, &context.output, clock.clone(), entry)?;
        let last_upload = reader.last_upload.clone();
        let mut progress = Progress::new(
            context,
            clock.clone(),
            slot,
            source.dataset.size,
            Operation::Fetch { approval: false },
        );
        let result =
            client.upload_dataset(&source.dataset, &mut reader, context.timing(), |stage| {
                if matches!(
                    stage,
                    darkbio_connect::UploadProgress::Started { .. }
                        | darkbio_connect::UploadProgress::Uploading { .. }
                ) {
                    last_upload.set(Some(clock.now()));
                }
                progress.update(stage);
            });
        context.interrupt.clear();
        let corrupt = matches!(&result, Err(darkbio_connect::Error::Integrity(_)));
        let failed = reader.failed;
        let result = result.map_err(|error| {
            reader.error.take().unwrap_or_else(|| match error {
                darkbio_connect::Error::DatasetRead(error) if failed => http::read_error(error),
                error => error.into(),
            })
        });
        if let Err(error) = &result {
            context.output.fail_stage(Some(error));
        } else {
            context.output.end_stage(None, None);
        }

        // A verified download is reusable even if the Ark later rejects processing.
        // Cache completion describes the source, not the slot's resulting state.
        let valid = reader.read == source.dataset.size
            && reader.hash.clone().finalize().as_slice() == source.dataset.sha256.unwrap();
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
            Err(_) if corrupt && reader.offset > 0 && attempt < 2 => context.output.event(
                "warning",
                "cached prefix failed verification; restarting download",
            ),
            Err(_) if failed && attempt < 2 => context.output.event(
                "warning",
                format!("download interrupted; retry {}/3", attempt + 2),
            ),
            Err(error) => return Err(error),
        }
    }
    unreachable!("last attempt returns its result")
}

/// Uploads a local copy once, leaving any failure for the caller to report.
fn replay(
    context: &Context,
    client: &Client,
    source: &Source,
    file: &mut File,
) -> Result<(), darkbio_connect::Error> {
    let mut progress = Progress::new(
        context,
        client.clock(),
        source.dataset.slot.expect("download target slot"),
        source.dataset.size,
        Operation::Fetch {
            approval: source.personal,
        },
    );
    let result = client.upload_dataset(&source.dataset, file, context.timing(), |stage| {
        progress.update(stage)
    });
    context.interrupt.clear();
    if result.is_ok() {
        context.output.end_stage(None, None);
    }
    result
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
    /// Session start or latest upload acknowledgment, shared with progress
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
            // Reset the cache so the next attempt starts from zero
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
        // acknowledgment
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
    mod personal;

    use super::*;
    use clap::Parser;
    use darkbio_clock::TestClock;
    use darkbio_connect::schema::{SlotDownload, SlotKind, SlotOrigin, SlotState};
    use std::io::Write;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    /// Parses a sample the way the command line does.
    fn sample(value: &str) -> Sample {
        let cli = crate::args::Cli::parse_from([
            "ark",
            "data",
            "fetch",
            "snp-indel-calls",
            "--sample",
            value,
        ]);
        cli.validate().unwrap();
        let Some(crate::args::Command::Data(crate::args::Data::Fetch {
            sample: Some(sample),
            ..
        })) = cli.command
        else {
            panic!("sample fetch command");
        };
        sample
    }

    /// Builds an empty snp-indel-calls slot offering the sample's pinned file.
    fn sample_slot(sample: Sample) -> SlotStatus {
        SlotStatus {
            kind: SlotKind::SnpIndelCalls as i32,
            origin: SlotOrigin::Personal as i32,
            state: SlotState::Empty as i32,
            download: Some(sample.download()),
            ..Default::default()
        }
    }

    /// Checks HG00096's source against its separately measured size and digest.
    #[test]
    fn test_sample_source_matches_independent_pin() {
        for value in ["HG00096", "notable-owen"] {
            let source = offer(&sample_slot(sample(value))).unwrap();
            assert_eq!(
                source.url,
                "https://1000genomes-dragen-v4-4-7.s3.amazonaws.com/data/individuals/hg38-alt_masked.cnv.graph.hla.methyl_cg.rna-11-r5.0-2/HG00096/HG00096.hard-filtered.vcf.gz",
                "{value} URL"
            );
            assert_eq!(
                source.dataset.name, "HG00096.hard-filtered.vcf.gz",
                "{value} filename"
            );
            assert_eq!(source.dataset.size, 437_027_190, "{value} size");
            assert_eq!(source.dataset.slot, Some(3), "{value} target");
            assert_eq!(
                source.hash, "488f8b0fc05cfae2a2c7b4208367abefa8401fcef3151e32feb5587d71303260",
                "{value} digest"
            );
            assert_eq!(
                hex::encode(source.dataset.sha256.unwrap()),
                "488f8b0fc05cfae2a2c7b4208367abefa8401fcef3151e32feb5587d71303260",
                "{value} upload digest"
            );
        }
    }

    /// Checks that only snp-indel-calls points a missing download at samples.
    #[test]
    fn test_no_download_suggests_samples_only_for_variant_calls() {
        for id in [1, 2, 3, 4, 42] {
            let error = plan(&[reference(id, &[], false)], Some(id)).unwrap_err();
            assert_eq!((error.class, error.code), (5, "no-download"), "slot {id}");
            assert_eq!(
                error.hints,
                if id == 3 {
                    vec!["--sample fills it with a public genome from `ark data samples`"]
                } else {
                    vec![]
                },
                "slot {id} guidance"
            );
        }
    }

    /// Runs a sample plan that must install nothing, and checks its output.
    fn check_sample_plan(test: &str, filled: bool, dry_run: bool) {
        for captured in crate::testing::narration(test, |output, _| {
            let sample = sample("notable-owen");
            let mut slot = sample_slot(sample);
            if filled {
                slot.state = SlotState::Filled as i32;
            }
            let slots = [slot];
            let ordered = plan(&slots, Some(3)).unwrap();
            let context = Context {
                options: crate::args::Cli::parse_from(["ark"]).options,
                interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                output,
            };
            execute(&context, &ordered, Some(sample), dry_run, None, |_| {
                panic!("a dry run or filled slot never installs a sample")
            })
            .unwrap();
        }) {
            if captured.json {
                let document: Value = serde_json::from_str(&captured.stdout).unwrap();
                assert_eq!(
                    document["fetched"].as_array().unwrap().len(),
                    1,
                    "one selected slot"
                );
                let row = &document["fetched"][0];
                assert_eq!(row["sample"], "HG00096", "selected sample");
                assert_eq!(row["name"], "notable-owen", "selected name");
                assert_eq!(row["size_bytes"], 437_027_190, "pinned file length");
                assert_eq!(
                    row["sha256"],
                    "488f8b0fc05cfae2a2c7b4208367abefa8401fcef3151e32feb5587d71303260",
                    "pinned digest"
                );
                assert_eq!(
                    row["outcome"],
                    if filled { "skipped" } else { "planned" },
                    "plan outcome"
                );
            } else {
                assert_eq!(
                    captured
                        .stdout
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .collect::<Vec<_>>(),
                    ["SLOT", "SIZE", "CACHED", "OUTCOME"],
                    "fetch columns stay unchanged"
                );
            }
            if filled {
                captured.contains(&["snp-indel-calls is already filled"]);
            } else {
                assert!(
                    captured.stderr.is_empty(),
                    "dry run has no download or approval events"
                );
            }
        }
    }

    /// Checks that a sample dry run plans the pinned file and moves nothing.
    #[test]
    fn test_sample_dry_run_only_plans() {
        check_sample_plan(
            "data::reference::tests::test_sample_dry_run_only_plans",
            false,
            true,
        );
    }

    /// Checks that a filled slot keeps its data and skips the sample.
    #[test]
    fn test_sample_fetch_skips_a_filled_slot() {
        check_sample_plan(
            "data::reference::tests::test_sample_fetch_skips_a_filled_slot",
            true,
            false,
        );
    }

    /// Checks a sample fetch end to end, from download to processing and cache.
    #[test]
    fn test_sample_fetch_downloads_processes_and_caches() {
        use darkbio_connect::{
            Ark, TrustMode, schema,
            trust::{Environment, Realm},
        };
        use schema::host_to_ark::Content;
        use std::sync::{Arc, Mutex};

        for captured in crate::testing::narration(
            "data::reference::tests::test_sample_fetch_downloads_processes_and_caches",
            |output, clock| {
                // Serve a small invented file from the scripted HTTP server
                let bytes = b"tiny variant-call fixture\n";
                let digest = "df9a83175f9bf0ab03383500321104f6bd1ae0367631ce81c2909d7813f68a79";
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let url = format!(
                    "http://{}/HG00096.hard-filtered.vcf.gz",
                    listener.local_addr().unwrap()
                );
                let server = serve(listener, vec![(200, String::new(), bytes.to_vec())]);
                let directory = Directory::new();
                let sample = sample("notable-owen");
                let mut slot = sample_slot(sample);
                let download = slot.download.as_mut().unwrap();
                download.bytes = bytes.len() as u64;
                download.sha256 = digest.into();

                // Script an Ark that takes the upload and fills the slot
                let stored = Arc::new(Mutex::new(slot.clone()));
                let mut peer = crate::testing::peer::Peer::spawn(
                    &clock.clock(),
                    Box::new({
                        let stored = stored.clone();
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
                                    assert_eq!(
                                        request.name, "HG00096.hard-filtered.vcf.gz",
                                        "sample filename sent to Ark"
                                    );
                                    assert_eq!(request.kind, 3, "personal slot target");
                                    assert_eq!(request.size, 26, "fixture upload size");
                                    assert_eq!(
                                        request.chunk, bytes,
                                        "downloaded bytes reach the Ark"
                                    );
                                    responder
                                        .reply(
                                            schema::SlotUploadStartResponse { session: 7 },
                                            deadline,
                                        )
                                        .unwrap();
                                }
                                Content::SlotUploadProcess(request) => {
                                    assert_eq!(request.session, 7, "upload session to process");
                                    let mut slot = stored.lock().unwrap();
                                    slot.state = SlotState::Filled as i32;
                                    slot.bytes = 26;
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
                                Content::SlotList(_) => {
                                    responder
                                        .reply(
                                            schema::SlotListResponse {
                                                slots: vec![stored.lock().unwrap().clone()],
                                            },
                                            deadline,
                                        )
                                        .unwrap();
                                }
                                _ => panic!("unexpected sample fetch request: {request:?}"),
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
                let client = ark.client();
                let context = Context {
                    options: crate::args::Cli::parse_from(["ark"]).options,
                    interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                    output,
                };

                // Plan and install, swapping only the URL for the test server
                let slots = [slot];
                let ordered = plan(&slots, Some(3)).unwrap();
                let agent = agent();
                execute(
                    &context,
                    &ordered,
                    Some(sample),
                    false,
                    Some(&directory.0),
                    |source| {
                        let source = Source {
                            url: url.clone(),
                            dataset: source.dataset.clone(),
                            hash: source.hash.clone(),
                            personal: source.personal,
                        };
                        install(&context, &client, &source, Some(&directory.0), &agent)
                    },
                )
                .unwrap();

                // Read back the processed slot and the cached file
                let slots = client
                    .call(schema::SlotListRequest {}, context.timing())
                    .unwrap()
                    .slots;
                assert_eq!(
                    slots[0].state,
                    SlotState::Filled as i32,
                    "sample slot is processed"
                );
                assert_eq!(slots[0].bytes, 26, "processed fixture length");
                assert_eq!(
                    fs::read(directory.0.join(digest)).unwrap(),
                    bytes,
                    "verified cache bytes"
                );
                let requests = server.join().unwrap();
                assert_eq!(requests.len(), 1, "one HTTP download");
                assert!(
                    requests[0].starts_with("get /hg00096.hard-filtered.vcf.gz http/1.1\r\n"),
                    "sample request path"
                );
                drop(ark);
            },
        ) {
            captured.contains(&[
                "Approve the upload on your phone, in Ark Companion",
                "Approved on your phone",
                "Fetched snp-indel-calls",
                "Indexing",
                "cache:",
            ]);
            captured.approvals(&[("upload", Some("phone"))]);
            if captured.json {
                let document: Value = serde_json::from_str(&captured.stdout).unwrap();
                assert_eq!(
                    document["fetched"][0],
                    json!({
                        "slot": "snp-indel-calls", "id": 3,
                        "url": "https://1000genomes-dragen-v4-4-7.s3.amazonaws.com/data/individuals/hg38-alt_masked.cnv.graph.hla.methyl_cg.rna-11-r5.0-2/HG00096/HG00096.hard-filtered.vcf.gz",
                        "size_bytes": 26,
                        "sha256": "df9a83175f9bf0ab03383500321104f6bd1ae0367631ce81c2909d7813f68a79",
                        "cached": false, "outcome": "done", "error": null,
                        "sample": "HG00096", "name": "notable-owen",
                    }),
                    "completed sample fetch row"
                );
            }
        }
    }

    /// A fetch with nothing to fetch reads as its sentence, and JSON keeps the
    /// empty list.
    #[test]
    fn test_fetch_without_reference_slots() {
        for captured in crate::testing::narration(
            "data::reference::tests::test_fetch_without_reference_slots",
            |output, _| {
                // Plan from an inventory that holds only personal data
                let slots = [SlotStatus {
                    kind: SlotKind::SnpIndelCalls as i32,
                    origin: SlotOrigin::Personal as i32,
                    ..Default::default()
                }];
                let ordered = plan(&slots, None).unwrap();
                let context = Context {
                    options: crate::args::Cli::parse_from(["ark"]).options,
                    interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                    output,
                };

                // Print the empty plan, which installs nothing
                let result = execute(&context, &ordered, None, false, None, |_| {
                    panic!("an empty reference plan has no installation")
                });
                assert!(result.is_ok(), "empty fetch succeeds: {result:?}");
            },
        ) {
            assert_eq!(
                captured.stdout,
                if captured.json {
                    "{\n  \"fetched\": []\n}\n"
                } else {
                    "No reference datasets to fetch\n"
                },
                "json={}, terminal={}",
                captured.json,
                captured.terminal,
            );
            assert_eq!(captured.stderr, "", "empty fetch has no events");
        }
    }

    /// Counter that keeps parallel fixtures in separate temporary directories.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Temporary cache directory, removed with its files on drop.
    struct Directory(
        /// Path of the temporary directory.
        PathBuf,
    );

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
        /// Tries to remove the directory and its contents, ignoring failures.
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
            personal: false,
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
            origin: SlotOrigin::Reference as i32,
            deps: deps.into(),
            state: if filled {
                SlotState::Filled
            } else {
                SlotState::Empty
            } as i32,
            ..Default::default()
        }
    }

    /// Builds an empty reference slot with an invented, valid download offer.
    fn offered(id: i32, deps: &[i32]) -> SlotStatus {
        SlotStatus {
            download: Some(SlotDownload {
                url: "https://example.test/reference.gz".into(),
                bytes: 10,
                sha256: "ab".repeat(32),
            }),
            ..reference(id, deps, false)
        }
    }

    /// The plan keeps filled slots in dependency order, and refuses cycles and
    /// missing dependencies of empty slots.
    #[test]
    fn dependency_order_retains_filled_slots_and_detects_missing_inputs() {
        let slots = [
            reference(3, &[1], false),
            offered(1, &[0]),
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

    /// Fetching all references skips missing offers, preserves dependencies
    /// and reports the same skips in a dry run.
    #[test]
    fn test_fetch_skips_unavailable_references() {
        // Child scenarios use the command's planner and executor, recording
        // installations instead of opening a device or fetching public data
        if let Ok(scenario) = std::env::var("ARK_TEST_FETCH_SCENARIO") {
            let options = crate::args::Cli::parse_from(["ark", "--json"]).options;
            let output = Output::new(&options);
            let context = Context {
                options,
                interrupt: crate::interrupt::Interrupt::install(output.clone()).unwrap(),
                output,
            };
            let directory = Directory::new();
            let mut slots = [
                offered(2, &[3, 1]),
                reference(4, &[3], false),
                offered(1, &[3]),
                SlotStatus {
                    origin: SlotOrigin::Personal as i32,
                    ..reference(3, &[], true)
                },
            ];
            let mut id = None;
            let dry_run = scenario == "dry-run";
            match scenario.as_str() {
                "all" | "dry-run" | "failed" => {}
                "named" => id = Some(4),
                "missing-calls" => {
                    slots[3].state = SlotState::Empty as i32;
                    for slot in &mut slots {
                        slot.download = None;
                    }
                }
                "skipped-dependency" => slots[0].deps = vec![4],
                "filled" => slots[1].state = SlotState::Filled as i32,
                "malformed" => slots[0].download.as_mut().unwrap().sha256 = "bad".into(),
                _ => panic!("unexpected scenario {scenario}"),
            }

            // Planning errors precede all installations, and a transfer failure
            // retains the completed plan's skipped and unattempted rows
            let mut installed = Vec::new();
            writeln!(std::io::stdout(), "<result>").unwrap();
            let result = plan(&slots, id).and_then(|ordered| {
                execute(
                    &context,
                    &ordered,
                    None,
                    dry_run,
                    Some(&directory.0),
                    |source| {
                        installed.push(source.dataset.slot.unwrap());
                        assert_eq!(source.url, "https://example.test/reference.gz");
                        assert_eq!(source.dataset.size, 10);
                        assert_eq!(source.dataset.sha256, Some([0xab; 32]));
                        if scenario == "failed" {
                            Err(Error::new(5, "ark", "reference upload refused"))
                        } else {
                            Ok(())
                        }
                    },
                )
            });
            let expected = match scenario.as_str() {
                "all" | "filled" => vec![1, 2],
                "failed" => vec![1],
                _ => vec![],
            };
            assert_eq!(installed, expected);
            let code = match result {
                Ok(()) => 0,
                Err(error) => {
                    if !context.output.printed() {
                        context
                            .output
                            .document(&json!({"error": error.json()}))
                            .unwrap();
                    }
                    context.output.error(&error);
                    error.class
                }
            };
            writeln!(std::io::stdout(), "</result>").unwrap();
            drop(directory);
            std::process::exit(i32::from(code));
        }

        // Capture exact documents, note events and exit classes at the output
        // boundary, including planning failures before any partial result
        for (scenario, code, error) in [
            ("all", 0, None),
            ("dry-run", 0, None),
            ("named", 5, Some("no-download")),
            ("missing-calls", 5, Some("dependency-missing")),
            ("skipped-dependency", 5, Some("dependency-missing")),
            ("filled", 0, None),
            ("malformed", 1, Some("file-rejected")),
            ("failed", 5, Some("ark")),
        ] {
            let captured = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "data::reference::tests::test_fetch_skips_unavailable_references",
                    "--nocapture",
                ])
                .env("ARK_TEST_FETCH_SCENARIO", scenario)
                .output()
                .unwrap();
            assert_eq!(
                captured.status.code(),
                Some(code),
                "{scenario}: {captured:?}"
            );
            let stdout = String::from_utf8(captured.stdout).unwrap();
            let document = stdout
                .split_once("<result>\n")
                .unwrap()
                .1
                .split_once("</result>\n")
                .unwrap()
                .0;
            let document: Value = serde_json::from_str(document).unwrap();
            let stderr = String::from_utf8(captured.stderr).unwrap();
            let events: Vec<Value> = stderr
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            if let Some(error) = error {
                assert_eq!(events.last().unwrap()["error"]["code"], error, "{scenario}");
                if scenario != "failed" {
                    assert_eq!(document["error"]["code"], error, "{scenario}");
                    assert_eq!(events.len(), 1, "{scenario}");
                    continue;
                }
            }

            // The unavailable catalog has no download metadata or cache hit;
            // the offered slots keep their dependency order and outcomes
            assert_eq!(
                document["fetched"][0],
                json!({
                    "slot": "variant-catalog", "id": 4, "url": null,
                    "size_bytes": null, "sha256": null, "cached": false,
                    "outcome": "skipped", "error": null,
                }),
                "{scenario}"
            );
            assert_eq!(document["fetched"].as_array().unwrap().len(), 3);
            for (index, slot) in [(1, "reference-genome"), (2, "gene-annotations")] {
                let row = &document["fetched"][index];
                assert_eq!(row["slot"], slot, "{scenario}");
                assert_eq!(
                    row["outcome"],
                    match scenario {
                        "dry-run" => "planned",
                        "failed" if index == 1 => "failed",
                        "failed" => "not-attempted",
                        _ => "done",
                    },
                    "{scenario}"
                );
            }
            assert_eq!(
                events[0],
                json!({
                    "event": "note",
                    "message": if scenario == "filled" {
                        "variant-catalog is already filled"
                    } else {
                        "variant-catalog offers no download"
                    },
                }),
                "{scenario}"
            );
            assert_eq!(
                events.len(),
                if scenario == "failed" { 2 } else { 1 },
                "{scenario}"
            );
        }
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
    /// acknowledgment.
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
