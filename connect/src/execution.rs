// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! App uploads, companion authorization and execution results.

use crate::{Error, schema};
use darkbio_wire::protocol::{Message, Promise, Requester};
use std::io::{self, Read};
use std::time::{Duration, Instant};

const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024; // Leave room for sealing and framing
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Execution stages reported on the caller's thread. A task ID identifies
/// both the upload and its eventual execution, including cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionProgress {
    /// Allocating an app upload on the Ark.
    Preparing,
    /// The Ark allocated this task; cancellation can now address it.
    Started { taskid: u64 },
    /// App bytes acknowledged by the Ark so far.
    Uploading { uploaded: u64, total: u64 },
    /// Requesting companion approval to run the uploaded app.
    Authorizing,
    /// The app is running. Elapsed time starts at the scheduling acknowledgement.
    Running { elapsed: Duration },
}

/// Streams at most two outstanding chunks, waits for authorization and retrieves
/// the result once. Scheduling establishes the relay through the caller's client.
pub(crate) fn execute(
    requester: &Requester,
    size: u64,
    reader: &mut impl Read,
    deadline: Instant,
    mut progress: impl FnMut(ExecutionProgress),
    schedule: impl FnOnce(u64) -> Result<(), Error>,
) -> Result<schema::ExecutionResultResponse, Error> {
    if size == 0 {
        return Err(Error::Execution("app is empty".into()));
    }
    progress(ExecutionProgress::Preparing);
    let taskid = requester
        .request(
            schema::ExecutionUploadStartRequest { bytes: size },
            deadline,
        )?
        .wait::<schema::ExecutionUploadStartResponse>()?
        .taskid;
    let result = (|| {
        progress(ExecutionProgress::Started { taskid });
        progress(ExecutionProgress::Uploading {
            uploaded: 0,
            total: size,
        });
        let mut sent = 0;
        let mut uploaded = 0;
        let mut pending: Option<(Promise<Message>, u64)> = None;
        while sent < size {
            remaining(deadline)?;
            let bytes = (size - sent).min(CHUNK_SIZE as u64) as usize;
            let mut chunk = vec![0; bytes];
            reader.read_exact(&mut chunk).map_err(read_error)?;
            remaining(deadline)?;
            sent += bytes as u64;
            if sent == size {
                finish_read(reader, deadline)?;
            }
            let next = requester.request(
                schema::ExecutionUploadChunkRequest { taskid, chunk },
                deadline,
            )?;
            if let Some((previous, bytes)) = pending.take() {
                previous.wait::<schema::ExecutionUploadChunkResponse>()?;
                uploaded += bytes;
                progress(ExecutionProgress::Uploading {
                    uploaded,
                    total: size,
                });
            }
            pending = Some((next, bytes as u64));
        }
        if let Some((last, bytes)) = pending {
            last.wait::<schema::ExecutionUploadChunkResponse>()?;
            uploaded += bytes;
            progress(ExecutionProgress::Uploading {
                uploaded,
                total: size,
            });
        }
        progress(ExecutionProgress::Authorizing);
        schedule(taskid)?;
        let started = Instant::now();
        loop {
            progress(ExecutionProgress::Running {
                elapsed: started.elapsed(),
            });
            let status = requester
                .request(schema::ExecutionStatusRequest { taskid }, deadline)?
                .wait::<schema::ExecutionStatusResponse>()?;
            match (status.pending, status.result) {
                (false, Some(result)) => return Ok(result),
                (true, None) => {}
                _ => return Err(Error::Execution("invalid execution status".into())),
            }
            std::thread::sleep(POLL_INTERVAL.min(remaining(deadline)?));
        }
    })();
    if result.is_err() {
        let cleanup = deadline.min(Instant::now() + Duration::from_secs(1));
        let _ = requester
            .request(schema::ExecutionCancelRequest { taskid }, cleanup)
            .and_then(|pending| pending.wait::<schema::ExecutionCancelResponse>());
    }
    result
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(Error::Timeout)
}

/// Check EOF before the final chunk so a growing or misdeclared source never
/// reaches scheduling. Interrupted reads do not indicate the end of a file.
fn finish_read(reader: &mut impl Read, deadline: Instant) -> Result<(), Error> {
    loop {
        remaining(deadline)?;
        match reader.read(&mut [0]) {
            Ok(0) => break,
            Ok(_) => return Err(Error::Execution("app exceeds its advertised size".into())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_error(error)),
        }
    }
    remaining(deadline)?;
    Ok(())
}

fn read_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut {
        Error::Timeout
    } else {
        Error::ExecutionRead(error)
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
    use std::sync::{Arc, Mutex, mpsc};

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct Observed {
        stages: Vec<&'static str>,
        size: u64,
        bytes: Vec<u8>,
    }

    fn result(success: bool) -> schema::ExecutionResultResponse {
        schema::ExecutionResultResponse {
            app_name: "test app".into(),
            app_version: "1.2.3".into(),
            success,
            stdout: vec![0, 255, 42],
            stderr: vec![254, 0],
        }
    }

    fn peer(
        fail: Option<&'static str>,
        reports: Vec<schema::ExecutionStatusResponse>,
    ) -> (Peer, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = observed.clone();
        let mut reports = VecDeque::from(reports);
        let peer = Peer::spawn(Box::new(move |_, request, responder| {
            let mut observed = shared.lock().unwrap();
            let (stage, response): (_, Message) = match request {
                Content::ExecUploadStart(request) => {
                    observed.size = request.bytes;
                    (
                        "start",
                        schema::ExecutionUploadStartResponse { taskid: 7 }.into(),
                    )
                }
                Content::ExecUploadChunk(request) => {
                    assert_eq!(request.taskid, 7);
                    assert!(request.chunk.len() <= CHUNK_SIZE);
                    observed.bytes.extend(request.chunk);
                    ("chunk", schema::ExecutionUploadChunkResponse {}.into())
                }
                Content::ExecSched(request) => {
                    assert_eq!(request.taskid, 7);
                    assert_eq!(observed.bytes.len() as u64, observed.size);
                    ("schedule", schema::ExecutionScheduleResponse {}.into())
                }
                Content::ExecStatus(request) => {
                    assert_eq!(request.taskid, 7);
                    (
                        "status",
                        reports.pop_front().expect("unexpected status poll").into(),
                    )
                }
                Content::ExecCancel(request) => {
                    assert_eq!(request.taskid, 7);
                    ("cancel", schema::ExecutionCancelResponse {}.into())
                }
                _ => panic!("unexpected request"),
            };
            observed.stages.push(stage);
            let deadline = Instant::now() + TIMEOUT;
            if fail == Some(stage) || stage == "cancel" && fail.is_some() {
                responder
                    .fail(
                        schema::Error::new(0x778, format!("refused {stage}")),
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

    fn attach(peer: &mut Peer) -> Session {
        let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
        protocol::connect(peer.stream(), &trust).unwrap().0
    }

    fn run(
        requester: &Requester,
        size: u64,
        reader: &mut impl Read,
        progress: impl FnMut(ExecutionProgress),
    ) -> Result<schema::ExecutionResultResponse, Error> {
        let deadline = Instant::now() + TIMEOUT;
        execute(requester, size, reader, deadline, progress, |taskid| {
            requester
                .request(schema::ExecutionScheduleRequest { taskid }, deadline)?
                .wait::<schema::ExecutionScheduleResponse>()?;
            Ok(())
        })
    }

    /// The final acknowledgement precedes scheduling. Polling stops as soon as
    /// the result is retrieved, retaining binary output even when the app failed.
    #[test]
    fn test_execution() {
        for (size, success) in [(17, false), (3 * CHUNK_SIZE + 29, true)] {
            let expected = result(success);
            let (mut peer, observed) = peer(
                None,
                vec![
                    schema::ExecutionStatusResponse {
                        pending: true,
                        result: None,
                    },
                    schema::ExecutionStatusResponse {
                        pending: false,
                        result: Some(expected.clone()),
                    },
                ],
            );
            let session = attach(&mut peer);
            let bytes: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut progress = Vec::new();
            let actual = run(
                &session.requester(),
                size as u64,
                &mut bytes.as_slice(),
                |stage| progress.push(stage),
            )
            .unwrap();
            assert_eq!(actual, expected);
            let observed = observed.lock().unwrap();
            assert_eq!(observed.bytes, bytes);
            assert_eq!(
                observed
                    .stages
                    .iter()
                    .filter(|stage| **stage == "status")
                    .count(),
                2
            );
            assert!(!observed.stages.contains(&"cancel"));
            let authorizing = progress
                .iter()
                .position(|stage| *stage == ExecutionProgress::Authorizing)
                .unwrap();
            assert_eq!(
                progress[authorizing - 1],
                ExecutionProgress::Uploading {
                    uploaded: size as u64,
                    total: size as u64
                }
            );
            assert_eq!(progress[1], ExecutionProgress::Started { taskid: 7 });
        }
    }

    /// A remote failure keeps its code and message even when cleanup fails too.
    /// A refused start has no task ID and must not attempt cancellation.
    #[test]
    fn test_refusals() {
        for fail in ["start", "chunk", "schedule", "status"] {
            let (mut peer, observed) = peer(
                Some(fail),
                vec![schema::ExecutionStatusResponse {
                    pending: false,
                    result: Some(result(true)),
                }],
            );
            let session = attach(&mut peer);
            let result = run(&session.requester(), 1, &mut [42].as_slice(), |_| {});
            assert!(
                matches!(result, Err(Error::Remote(error)) if error.code == 0x778 && error.msg == format!("refused {fail}"))
            );
            let observed = observed.lock().unwrap();
            assert_eq!(observed.stages.contains(&"cancel"), fail != "start");
            assert_eq!(
                observed
                    .stages
                    .iter()
                    .filter(|stage| **stage == fail)
                    .count(),
                1
            );
            if fail == "chunk" {
                assert!(!observed.stages.contains(&"schedule"));
            }
        }
    }

    /// A missing result or contradictory pending flag must not become success.
    #[test]
    fn test_invalid_status() {
        for report in [
            schema::ExecutionStatusResponse::default(),
            schema::ExecutionStatusResponse {
                pending: true,
                result: Some(result(true)),
            },
        ] {
            let (mut peer, observed) = peer(None, vec![report]);
            let session = attach(&mut peer);
            assert!(matches!(
                run(&session.requester(), 1, &mut [42].as_slice(), |_| {}),
                Err(Error::Execution(_))
            ));
            assert_eq!(observed.lock().unwrap().stages.last(), Some(&"cancel"));
        }
    }

    /// Declared lengths are enforced before scheduling, including sources that
    /// change after their first chunk. Read failures cancel the allocated task.
    #[test]
    fn test_source_length() {
        for size in [17, CHUNK_SIZE + 17] {
            for extra in [-1_i64, 1] {
                let (mut peer, observed) = peer(None, vec![]);
                let session = attach(&mut peer);
                let bytes = vec![42; (size as i64 + extra) as usize];
                assert!(
                    run(
                        &session.requester(),
                        size as u64,
                        &mut bytes.as_slice(),
                        |_| {}
                    )
                    .is_err()
                );
                let observed = observed.lock().unwrap();
                assert!(!observed.stages.contains(&"schedule"));
                assert_eq!(observed.stages.last(), Some(&"cancel"));
            }
        }
    }

    /// Two requests can be in flight, and acknowledgements can arrive reversed.
    /// The last chunk remains outstanding until explicitly released by the test.
    #[test]
    fn test_upload_window() {
        let (notice, notices) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut first = None;
        let mut chunks = 0;
        let mut peer = Peer::spawn(Box::new(move |_, request, responder| {
            let deadline = Instant::now() + TIMEOUT;
            match request {
                Content::ExecUploadStart(_) => {
                    responder
                        .reply(schema::ExecutionUploadStartResponse { taskid: 7 }, deadline)
                        .unwrap();
                }
                Content::ExecUploadChunk(_) => {
                    chunks += 1;
                    if chunks == 1 {
                        first = Some(responder);
                    } else {
                        if chunks == 3 {
                            notice.send(()).unwrap();
                            released.recv_timeout(TIMEOUT).unwrap();
                        }
                        responder
                            .reply(schema::ExecutionUploadChunkResponse {}, deadline)
                            .unwrap();
                        if chunks == 2 {
                            first
                                .take()
                                .unwrap()
                                .reply(schema::ExecutionUploadChunkResponse {}, deadline)
                                .unwrap();
                        }
                    }
                }
                Content::ExecSched(_) => {
                    responder
                        .reply(schema::ExecutionScheduleResponse {}, deadline)
                        .unwrap();
                }
                Content::ExecStatus(_) => {
                    responder
                        .reply(
                            schema::ExecutionStatusResponse {
                                pending: false,
                                result: Some(result(true)),
                            },
                            deadline,
                        )
                        .unwrap();
                }
                _ => panic!("unexpected request"),
            }
            true
        }));
        let session = attach(&mut peer);
        let requester = session.requester();
        let (updates, progress) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let bytes = vec![42; CHUNK_SIZE * 3];
            run(
                &requester,
                bytes.len() as u64,
                &mut bytes.as_slice(),
                |stage| {
                    updates.send(stage).unwrap();
                },
            )
        });
        notices.recv_timeout(TIMEOUT).unwrap();
        assert!(
            !progress
                .try_iter()
                .any(|stage| stage == ExecutionProgress::Authorizing)
        );
        release.send(()).unwrap();
        assert!(worker.join().unwrap().unwrap().success);
    }

    /// Cancellation can be sent while status is outstanding. Neither request
    /// requires an application receive loop or a second connection.
    #[test]
    fn test_cancellation() {
        let (notice, notices) = mpsc::channel();
        let mut held = None;
        let mut peer = Peer::spawn(Box::new(move |_, request, responder| {
            let deadline = Instant::now() + TIMEOUT;
            match request {
                Content::ExecUploadStart(_) => {
                    responder
                        .reply(schema::ExecutionUploadStartResponse { taskid: 7 }, deadline)
                        .unwrap();
                }
                Content::ExecUploadChunk(_) => {
                    responder
                        .reply(schema::ExecutionUploadChunkResponse {}, deadline)
                        .unwrap();
                }
                Content::ExecSched(_) => {
                    responder
                        .reply(schema::ExecutionScheduleResponse {}, deadline)
                        .unwrap();
                }
                Content::ExecStatus(_) => {
                    held = Some(responder);
                    notice.send(()).unwrap();
                }
                Content::ExecCancel(request) => {
                    assert_eq!(request.taskid, 7);
                    responder
                        .reply(schema::ExecutionCancelResponse {}, deadline)
                        .unwrap();
                    held.take()
                        .unwrap()
                        .reply(
                            schema::ExecutionStatusResponse {
                                pending: false,
                                result: Some(result(false)),
                            },
                            deadline,
                        )
                        .unwrap();
                }
                _ => panic!("unexpected request"),
            }
            true
        }));
        let session = attach(&mut peer);
        let requester = session.requester();
        let worker = std::thread::spawn(move || run(&requester, 1, &mut [42].as_slice(), |_| {}));
        notices.recv_timeout(TIMEOUT).unwrap();
        session
            .requester()
            .request(
                schema::ExecutionCancelRequest { taskid: 7 },
                Instant::now() + TIMEOUT,
            )
            .unwrap()
            .wait::<schema::ExecutionCancelResponse>()
            .unwrap();
        assert!(!worker.join().unwrap().unwrap().success);
    }

    /// Readers may fragment data or interrupt EOF checks. An expired deadline
    /// never allocates a task and a reader timeout remains a timeout error.
    #[test]
    fn test_reads_and_deadlines() {
        struct Fragmented {
            bytes: &'static [u8],
            interrupted: bool,
        }
        impl Read for Fragmented {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.bytes.is_empty() && !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(&mut buffer[..1])
            }
        }
        let (mut peer, _) = peer(
            None,
            vec![schema::ExecutionStatusResponse {
                pending: false,
                result: Some(result(true)),
            }],
        );
        let session = attach(&mut peer);
        let mut reader = Fragmented {
            bytes: &[1, 2, 3],
            interrupted: false,
        };
        assert!(
            run(&session.requester(), 3, &mut reader, |_| {})
                .unwrap()
                .success
        );
        assert!(reader.interrupted);

        let result = execute(
            &session.requester(),
            1,
            &mut [42].as_slice(),
            Instant::now(),
            |_| {},
            |_| panic!("expired execution scheduled"),
        );
        assert!(matches!(result, Err(Error::Timeout)));

        struct TimedOut;
        impl Read for TimedOut {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::TimedOut.into())
            }
        }
        assert!(matches!(
            run(&session.requester(), 1, &mut TimedOut, |_| {}),
            Err(Error::Timeout)
        ));
    }
}
