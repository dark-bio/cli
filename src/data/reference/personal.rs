// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Fetches into personal slots, which download and verify the whole file
//! before the one upload that asks the owner.

use super::{Reader, Source, cache, replay};
use crate::{
    args::slot_name,
    context::Context,
    error::Error,
    http,
    progress::{Transfer, Update},
};
use darkbio_clock::Clock;
use darkbio_connect::Client;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// A verified download, ready for the upload.
struct Prepared {
    /// The verified bytes, rewound for the upload to read from the start.
    file: File,
    /// Path of the cached copy, or none for a temporary file.
    cached: Option<PathBuf>,
}

/// Downloads and verifies the file, then uploads it once.
///
/// The upload asks the owner on their phone, so it is never retried. A failed
/// upload leaves the verified copy in the cache for the next run.
pub(super) fn install(
    context: &Context,
    client: &Client,
    source: &Source,
    directory: Option<&Path>,
    agent: &ureq::Agent,
) -> Result<(), Error> {
    let mut prepared = prepare(context, client.clock(), source, directory, agent)?;
    let result = replay(context, client, source, &mut prepared.file).map_err(Error::from);
    if let Err(error) = &result {
        context.output.fail_stage(Some(error));
    }
    if let Some(path) = prepared.cached {
        context
            .output
            .event("note", format!("cache: {}", path.display()));
    }
    result
}

/// Returns the cached copy if it verifies, or downloads the file in up to three
/// attempts.
fn prepare(
    context: &Context,
    clock: Clock,
    source: &Source,
    mut directory: Option<&Path>,
    agent: &ureq::Agent,
) -> Result<Prepared, Error> {
    // Verify a cached copy in full, since a bad one caught during the upload
    // would cost the owner an approval
    if let Some(directory) = directory {
        let path = directory.join(&source.hash);
        if let Ok(mut file) = File::open(&path) {
            if verified(&mut file, source).unwrap_or(false) {
                file.rewind()?;
                return Ok(Prepared {
                    file,
                    cached: Some(path),
                });
            }
            drop(file);
            context.output.event(
                "warning",
                "cached download failed verification; downloading it again",
            );
            let _ = fs::remove_file(&path);
        }
    }

    // Download into the cache, which keeps a broken attempt's bytes for the
    // next, or into a temporary file without one
    for attempt in 0..3 {
        let cached = directory.and_then(|directory| {
            match cache::Entry::open(directory, &source.hash, &source.url, source.dataset.size)
                .and_then(|entry| entry.prefix().map(|file| (entry, file)))
            {
                Ok(cached) => Some(cached),
                Err(_) => {
                    context.output.event(
                        "warning",
                        "cache unavailable or in use; downloading to a temporary file",
                    );
                    None
                }
            }
        });
        let (entry, mut file) = match cached {
            Some((entry, file)) => (Some(entry), file),
            None => {
                directory = None;
                (None, context.interrupt.temporary_file()?)
            }
        };
        let temporary = entry.is_none();
        let mut reader = Reader::new(agent, source, &context.output, clock.clone(), entry)?;
        let slot = slot_name(source.dataset.slot.expect("download target slot"));
        let mut progress = Transfer::new(
            context.output.terminal(),
            clock.clone(),
            Update {
                completed: Some(format!(
                    "Downloaded {slot} · {}",
                    crate::style::bytes(source.dataset.size)
                )),
                ..Update::new("downloading", &format!("Downloading {slot}"))
            },
        );
        context.output.progress(progress.initial());
        let mut result = receive(
            context,
            &mut reader,
            temporary.then_some(&mut file),
            &mut progress,
        );
        let corrupt = reader.read == source.dataset.size
            && reader.hash.clone().finalize().as_slice() != source.dataset.sha256.unwrap();

        // Publish a verified download in the cache, and drop a complete one that
        // failed, while a partial one stays to resume
        let entry = match reader.writer.take() {
            Some(writer) => writer.finish(result.is_ok()),
            None => reader.entry.take(),
        };
        let mut cached = None;
        if let Some(entry) = entry {
            if result.is_ok() {
                let path = entry
                    .path
                    .parent()
                    .expect("cache parent")
                    .join(&source.hash);
                match cache::complete(entry, &source.hash) {
                    Ok(()) => cached = Some(path),
                    Err(error) => context.output.event(
                        "warning",
                        format!("could not keep the download in the cache: {error}"),
                    ),
                }
            } else if reader.read >= source.dataset.size {
                let path = entry.path.clone();
                drop(entry);
                let _ = fs::remove_file(&path);
            }
        } else if !temporary {
            // The cache lost the bytes it was writing, so retry in a temporary file
            directory = None;
            result = Err(Error::new(1, "io", "could not keep the downloaded file"));
            if attempt < 2 {
                context.output.fail_stage(result.as_ref().err());
                context.output.event(
                    "warning",
                    "cache unavailable; retrying download in a temporary file",
                );
                continue;
            }
        }

        // End the download's stage before the upload opens its own
        match result {
            Ok(()) => {
                file.rewind()?;
                context.output.end_stage(None, None);
                return Ok(Prepared { file, cached });
            }
            Err(error) => {
                context.output.fail_stage(Some(&error));
                if corrupt && reader.offset > 0 && attempt < 2 {
                    context.output.event(
                        "warning",
                        "cached prefix failed verification; restarting download",
                    );
                } else if reader.failed && attempt < 2 {
                    context.output.event(
                        "warning",
                        format!("download interrupted; retry {}/3", attempt + 2),
                    );
                } else {
                    return Err(error);
                }
            }
        }
    }
    unreachable!("last attempt returns its result")
}

/// Checks a cached copy's size and SHA-256 against the pin.
fn verified(file: &mut File, source: &Source) -> io::Result<bool> {
    if file.metadata()?.len() != source.dataset.size {
        return Ok(false);
    }
    let mut hash = Sha256::new();
    let size = io::copy(
        &mut file.take(source.dataset.size.saturating_add(1)),
        &mut hash,
    )?;
    Ok(size == source.dataset.size && hash.finalize().as_slice() == source.dataset.sha256.unwrap())
}

/// Reads the download to its end, copying it into the temporary file if any.
///
/// It reads one byte past the pinned size to catch a longer file, and checks
/// the SHA-256 once the bytes end.
fn receive(
    context: &Context,
    reader: &mut Reader<'_>,
    mut temporary: Option<&mut File>,
    progress: &mut Transfer,
) -> Result<(), Error> {
    let mut buffer = [0; 64 * 1024];
    loop {
        let remaining = reader.source.dataset.size.saturating_sub(reader.read);
        let limit = remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let count = match reader.read(&mut buffer[..limit]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(reader.error.take().unwrap_or_else(|| {
                    if reader.failed {
                        http::read_error(error)
                    } else {
                        error.into()
                    }
                }));
            }
        };
        if reader.read > reader.source.dataset.size {
            return Err(Error::new(
                1,
                "file-rejected",
                "download exceeds the size built into ark",
            ));
        }
        if count == 0 {
            break;
        }
        if let Some(file) = &mut temporary {
            file.write_all(&buffer[..count])?;
        }
        if let Some(update) = progress.update(reader.read, reader.source.dataset.size) {
            context.output.progress(&update);
        }
    }
    if reader.hash.clone().finalize().as_slice() != reader.source.dataset.sha256.unwrap() {
        return Err(Error::new(
            1,
            "file-rejected",
            "download does not match the SHA-256 built into ark",
        ));
    }
    Ok(())
}
