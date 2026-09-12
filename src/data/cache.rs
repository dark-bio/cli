// ark: command line for Dark Bio Arks
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Public reference cache. Personal uploads and app results never enter it.

use crate::output::Output;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

pub(super) const CHUNK: usize = 1024 * 1024;

pub(crate) fn directory() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.cache_dir().join("ark"))
        .unwrap_or_else(|| std::env::temp_dir().join("ark-cache"))
}

/// Only a complete digest can name a cache entry; advertised metadata may
/// contain unknown or malformed values even when no download is requested.
pub(super) fn cached(directory: &Path, hash: &str) -> bool {
    let mut digest = [0; 32];
    hex::decode_to_slice(hash, &mut digest).is_ok() && directory.join(hex::encode(digest)).is_file()
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Metadata {
    pub url: String,
    pub validator: Option<String>,
    pub size: u64,
}

/// A separate lock file permits reading the retained prefix while the worker
/// appends. Locking the data file itself would prevent those reads on Windows.
/// The worker retains the lock until queued writes and the final sync finish.
pub(super) struct Entry {
    pub path: PathBuf,
    pub meta: Metadata,
    file: File,
    lock: File,
}
impl Entry {
    pub fn open(directory: &Path, hash: &str, url: &str, size: u64) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join(format!("{hash}.part"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.try_lock().map_err(io::Error::other)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let old = fs::read(path.with_extension("meta"))
            .ok()
            .and_then(|data| serde_json::from_slice::<Metadata>(&data).ok());
        let meta = old
            .filter(|meta| meta.url == url && meta.size == size)
            .unwrap_or(Metadata {
                url: url.into(),
                validator: None,
                size,
            });
        let length = file.metadata()?.len();
        if meta.validator.is_none() || length > size {
            file.set_len(0)?;
        } else {
            file.set_len(length / CHUNK as u64 * CHUNK as u64)?;
        }
        Ok(Self {
            path,
            meta,
            file,
            lock,
        })
    }
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    pub fn reset(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.rewind()?;
        Ok(())
    }
    pub fn save(&self) -> io::Result<()> {
        let data = serde_json::to_vec(&self.meta).map_err(io::Error::other)?;
        let sidecar = self.path.with_extension("meta");
        let temporary = sidecar.with_extension("meta.tmp");
        fs::write(&temporary, data)?;
        fs::rename(temporary, sidecar)
    }
    pub fn prefix(&self) -> io::Result<File> {
        File::open(&self.path)
    }
    pub fn writer(mut self, output: Output) -> io::Result<Writer> {
        self.file.seek(io::SeekFrom::End(0))?;
        let failed = Arc::new(AtomicBool::new(false));
        let fault = failed.clone();
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(2);
        let path = self.path.clone();
        let worker = std::thread::Builder::new()
            .name("ark-cache".into())
            .spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    if self
                        .file
                        .write_all(&bytes)
                        .and_then(|()| self.file.sync_data())
                        .is_err()
                    {
                        fault.store(true, Ordering::SeqCst);
                        output.event(
                            "warning",
                            "cache write failed; continuing without a retained copy",
                        );
                        let path = self.path.clone();
                        drop(self);
                        let _ = fs::remove_file(&path);
                        let _ = fs::remove_file(path.with_extension("meta"));
                        return None;
                    }
                }
                Some(self)
            })?;
        Ok(Writer {
            sender: Some(sender),
            worker: Some(worker),
            buffer: Vec::with_capacity(CHUNK),
            failed,
            path,
        })
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        // Release explicitly: a concurrently spawned child may briefly inherit
        // the descriptor before exec closes it.
        let _ = self.lock.unlock();
    }
}

pub(super) struct Writer {
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    worker: Option<std::thread::JoinHandle<Option<Entry>>>,
    buffer: Vec<u8>,
    failed: Arc<AtomicBool>,
    pub path: PathBuf,
}
impl Writer {
    pub fn append(&mut self, mut bytes: &[u8]) {
        if self.failed.load(Ordering::SeqCst) {
            return;
        }
        while !bytes.is_empty() {
            let count = bytes.len().min(CHUNK - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.buffer.len() == CHUNK {
                self.flush();
            }
        }
    }
    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(CHUNK));
        if let Some(sender) = &self.sender {
            let _ = sender.send(bytes);
        }
    }
    pub fn finish(mut self, complete: bool) -> Option<Entry> {
        if complete {
            self.flush();
        }
        self.sender.take();
        self.worker
            .take()
            .and_then(|worker| worker.join().ok().flatten())
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn complete(entry: Entry, hash: &str) -> io::Result<()> {
    let target = entry.path.parent().expect("cache parent").join(hash);
    fs::rename(&entry.path, target)?;
    let _ = fs::remove_file(entry.path.with_extension("meta"));
    Ok(())
}

pub(crate) fn size(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    fs::read_dir(path)?.try_fold(0u64, |total, entry| {
        let entry = entry?;
        Ok(total.saturating_add(entry.metadata()?.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Read;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ark-cache-test-{}-{}",
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
    fn output() -> Output {
        Output::new(&crate::args::Cli::parse_from(["ark", "--quiet"]).options)
    }
    fn entry(directory: &Directory) -> Entry {
        let mut entry = Entry::open(
            &directory.0,
            "hash",
            "https://example.com/reference",
            4 * CHUNK as u64,
        )
        .unwrap();
        entry.meta.validator = Some("\"version-1\"".into());
        entry.save().unwrap();
        entry
    }

    /// Only complete durable chunks survive a failed attempt; the disk worker
    /// retains the exclusive lock until all queued writes finish.
    #[test]
    fn interrupted_writer_retains_a_resumable_prefix() {
        let directory = Directory::new();
        let mut writer = entry(&directory).writer(output()).unwrap();
        writer.append(&vec![42; CHUNK + 17]);
        assert!(
            Entry::open(
                &directory.0,
                "hash",
                "https://example.com/reference",
                4 * CHUNK as u64
            )
            .is_err()
        );
        drop(writer.finish(false).unwrap());
        let resumed = Entry::open(
            &directory.0,
            "hash",
            "https://example.com/reference",
            4 * CHUNK as u64,
        )
        .unwrap();
        assert_eq!(resumed.len().unwrap(), CHUNK as u64);
        let mut bytes = Vec::new();
        resumed.prefix().unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![42; CHUNK]);
    }

    /// Incomplete bytes and a changed URL cannot be replayed under old metadata.
    #[test]
    fn resume_discards_uncommitted_or_unvalidated_bytes() {
        let directory = Directory::new();
        let entry = entry(&directory);
        entry.file.set_len(CHUNK as u64 + 91).unwrap();
        drop(entry);
        let resumed = Entry::open(
            &directory.0,
            "hash",
            "https://example.com/reference",
            4 * CHUNK as u64,
        )
        .unwrap();
        assert_eq!(resumed.len().unwrap(), CHUNK as u64);
        drop(resumed);
        let changed = Entry::open(
            &directory.0,
            "hash",
            "https://example.com/changed",
            4 * CHUNK as u64,
        )
        .unwrap();
        assert_eq!(changed.len().unwrap(), 0);
        assert!(changed.meta.validator.is_none());
    }

    #[test]
    fn complete_file_includes_the_last_partial_chunk() {
        let directory = Directory::new();
        let mut writer = entry(&directory).writer(output()).unwrap();
        writer.append(&vec![17; CHUNK + 3]);
        complete(writer.finish(true).unwrap(), "hash").unwrap();
        assert_eq!(
            fs::read(directory.0.join("hash")).unwrap(),
            vec![17; CHUNK + 3]
        );
        assert!(!directory.0.join("hash.part").exists());
        assert!(!directory.0.join("hash.meta").exists());
    }

    /// A failed cache write only disables retention. It must not turn the
    /// source reader into a failed Ark upload or deadlock the producer.
    #[test]
    fn disk_failure_releases_the_writer() {
        let directory = Directory::new();
        let mut entry = entry(&directory);
        entry.file = File::open(&entry.path).unwrap();
        let mut writer = entry.writer(output()).unwrap();
        writer.append(&vec![1; CHUNK * 5]);
        assert!(writer.finish(true).is_none());
        assert!(!directory.0.join("hash.part").exists());
    }
}
