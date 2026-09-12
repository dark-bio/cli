// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Download planning, transport recovery and cache replay.

use super::{Progress, cache, download, filled, select};
use crate::{
    args::slot_name,
    context::{Connection, Context},
    error::Error,
    http,
};
use darkbio_connect::{Dataset, schema::SlotStatus};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(super) fn fetch(
    context: &Context,
    connection: &Connection,
    slots: &[SlotStatus],
    id: Option<i32>,
    dry_run: bool,
    directory: Option<&Path>,
    no_cache: bool,
) -> Result<(), Error> {
    let directory = directory
        .map(Path::to_path_buf)
        .unwrap_or_else(cache::directory);
    let ordered = plan(slots, id)?;
    for slot in ordered.iter().filter(|slot| !filled(slot)) {
        offer(slot)?;
    }
    let mut rows:Vec<Value>=ordered.iter().map(|slot| {
        let offer=download(slot);
        json!({"slot":slot_name(slot.kind),"id":slot.kind,"url":offer.map(|o|o.0),"size_bytes":offer.map(|o|o.1),"sha256":offer.map(|o|o.2),
            "cached":!no_cache && offer.is_some_and(|o|cache::cached(&directory, o.2)),
            "outcome":if filled(slot) {"skipped"} else if dry_run {"planned"} else {"not-attempted"},"error":null})
    }).collect();
    context.interrupt.partial(json!({"fetched":rows}));
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

/// Dependencies determine order even when future slot kinds are addressed by id.
fn plan(slots: &[SlotStatus], id: Option<i32>) -> Result<Vec<&SlotStatus>, Error> {
    if let Some(id) = id {
        return Ok(vec![select(slots, id)?]);
    }
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

struct Source {
    dataset: Dataset,
    url: String,
    hash: String,
}
fn offer(slot: &SlotStatus) -> Result<Source, Error> {
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

fn install(
    context: &Context,
    connection: &Connection,
    source: &Source,
    directory: Option<&Path>,
) -> Result<(), Error> {
    let mut directory = directory;
    if let Some(directory) = directory {
        let path = directory.join(&source.hash);
        if let Ok(mut file) = File::open(&path) {
            let mut progress = Progress::new(context, source.dataset.slot.expect("reference slot"));
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
    let agent = http::agent(Duration::from_secs(context.options.timeout), 5);
    for attempt in 0..3 {
        let mut entry = match directory {
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
        let mut offset = entry
            .as_ref()
            .map(|entry| entry.len())
            .transpose()?
            .unwrap_or(0);
        let request = |offset: u64, entry: &Option<cache::Entry>| {
            let mut request = agent.get(&source.url).header("Accept-Encoding", "identity");
            if offset > 0 {
                request = request.header("Range", format!("bytes={offset}-"));
                if let Some(validator) = entry
                    .as_ref()
                    .and_then(|entry| entry.meta.validator.as_ref())
                {
                    request = request.header("If-Range", validator);
                }
            }
            request.call().map_err(http::error)
        };
        let mut response = match request(offset, &entry) {
            Ok(response) => response,
            Err(error) if attempt < 2 && (error.class == 4 || error.class == 7) => {
                context.output.event(
                    "warning",
                    format!("download interrupted; retry {}/3", attempt + 2),
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        if offset > 0 && !valid_range(&response, offset, source.dataset.size) {
            if let Some(entry) = &mut entry {
                entry.reset()?;
            }
            offset = 0;
            response = match request(0, &entry) {
                Ok(response) => response,
                Err(error) if attempt < 2 && (error.class == 4 || error.class == 7) => {
                    context.output.event(
                        "warning",
                        format!("download interrupted; retry {}/3", attempt + 2),
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
        }
        if (offset == 0 && response.status() != 200) || (offset > 0 && response.status() != 206) {
            return Err(Error::new(
                4,
                "cloud-unreachable",
                format!("reference download returned HTTP {}", response.status()),
            ));
        }
        if let Some(entry) = &mut entry {
            entry.meta.validator = response
                .headers()
                .get("ETag")
                .or_else(|| response.headers().get("Last-Modified"))
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let Err(error) = entry.save() {
                context
                    .output
                    .event("warning", format!("cannot save resume metadata: {error}"));
            }
        }
        let prefix = entry
            .as_ref()
            .filter(|_| offset > 0)
            .map(|entry| entry.prefix())
            .transpose()?;
        let writer = entry
            .map(|entry| entry.writer(context.output.clone()))
            .transpose()?;
        let last_upload = Rc::new(Cell::new(None));
        let mut reader = Reader {
            last_upload: last_upload.clone(),
            prefix: prefix.map(|file| file.take(offset)),
            network: response.body_mut().as_reader(),
            writer,
            hash: Sha256::new(),
            read: 0,
            failed: false,
            expected: source.dataset.size,
        };
        let mut progress = Progress::new(context, source.dataset.slot.expect("reference slot"));
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
                    last_upload.set(Some(Instant::now()));
                }
                progress.update(stage);
            },
        );
        context.interrupt.clear();
        let valid = reader.read == source.dataset.size
            && reader.hash.clone().finalize().as_slice() == source.dataset.sha256.unwrap();
        let failed = reader.failed;
        if let Some(writer) = reader.writer.take() {
            let path = writer.path.clone();
            if let Some(entry) = writer.finish(valid) {
                if valid {
                    if let Err(error) = cache::complete(entry, &source.hash) {
                        context.output.event(
                            "warning",
                            format!("could not retain complete cache entry: {error}"),
                        );
                    }
                } else if reader.read == source.dataset.size {
                    drop(entry);
                    let _ = fs::remove_file(&path);
                }
            } else {
                directory = None;
            }
        }
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
            Err(darkbio_connect::Error::Integrity(_)) if offset > 0 && attempt < 2 => {
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
                return Err(match error {
                    darkbio_connect::Error::Timeout => error.into(),
                    darkbio_connect::Error::DatasetRead(error) => http::read_error(error),
                    error => error.into(),
                });
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("last attempt returns its result")
}

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

struct Reader<R> {
    last_upload: Rc<Cell<Option<Instant>>>,
    prefix: Option<io::Take<File>>,
    network: R,
    writer: Option<cache::Writer>,
    hash: Sha256,
    read: u64,
    failed: bool,
    expected: u64,
}
impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some(prefix) = &mut self.prefix {
            let count = prefix.read(buffer)?;
            if count > 0 {
                self.hash.update(&buffer[..count]);
                self.read += count as u64;
                return Ok(count);
            }
            self.prefix = None;
        }
        let count = match self.network.read(buffer) {
            Ok(count) => count,
            Err(error) => {
                self.failed = true;
                return Err(http::normalize_read_error(error));
            }
        };
        if count == 0 && self.read < self.expected {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "reference download ended early",
            ));
        }
        if let Some(writer) = &mut self.writer {
            writer.append(&buffer[..count]);
        }
        self.hash.update(&buffer[..count]);
        self.read += count as u64;
        if self
            .last_upload
            .get()
            .is_some_and(|last| last.elapsed() >= Duration::from_secs(30))
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

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_connect::schema::{SlotDownload, SlotOrigin, SlotState};

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

    #[test]
    fn truncated_sources_are_transport_failures() {
        let mut reader = Reader {
            last_upload: Rc::new(Cell::new(None)),
            prefix: None,
            network: [1u8, 2].as_slice(),
            writer: None,
            hash: Sha256::new(),
            read: 0,
            expected: 3,
            failed: false,
        };
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(reader.failed);
        assert_eq!(reader.read, 2);
    }
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
    #[test]
    fn download_stall_tracks_the_upload_window() {
        let mut reader = Reader {
            last_upload: Rc::new(Cell::new(Some(Instant::now() - Duration::from_secs(31)))),
            prefix: None,
            network: [42].as_slice(),
            writer: None,
            hash: Sha256::new(),
            read: 0,
            expected: 1,
            failed: false,
        };
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(reader.failed);
    }
}
