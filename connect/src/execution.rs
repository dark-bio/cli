// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! App uploads, companion authorization and execution results.

use crate::{Error, Timing, schema};
use darkbio_clock::Clock;
use darkbio_wire::protocol::{Message, Promise, Requester};
use std::io::{self, Read};
use std::time::Duration;

/// Largest upload chunk, 32 KiB short of a 2 MiB frame to leave room for
/// sealing and framing.
const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024;
/// Delay between status requests while the Ark retains a pending task.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Execution stages reported on the caller's thread.
///
/// A task ID identifies both the upload and its eventual execution, including
/// cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionProgress {
    /// Allocating an app upload on the Ark.
    Preparing,
    /// The Ark allocated this task; cancellation can now address it.
    Started {
        /// Task ID accepted by [`schema::ExecutionCancelRequest`].
        taskid: u64,
    },
    /// App bytes acknowledged by the Ark so far.
    Uploading {
        /// App bytes acknowledged so far.
        uploaded: u64,
        /// Declared app length in bytes.
        total: u64,
    },
    /// Requesting companion approval to run the uploaded app.
    Authorizing,
    /// The app is running, its elapsed time counted from the scheduling
    /// acknowledgment.
    Running {
        /// Host time since scheduling succeeded, including status polling waits.
        elapsed: Duration,
    },
}

/// Uploads and runs an app, streaming at most two outstanding chunks, waiting
/// for authorization and retrieving the result once.
///
/// Scheduling establishes the relay through the caller's client. After a
/// failure it requests the task's cancellation within the remaining deadline,
/// at most 1 s, ignoring cancellation errors. Deadlines and the running time
/// are measured on the clock of the requester's session.
pub(crate) fn execute(
    requester: &Requester,
    size: u64,
    reader: &mut impl Read,
    timing: impl Into<Timing>,
    mut progress: impl FnMut(ExecutionProgress),
    schedule: impl FnOnce(u64) -> Result<(), Error>,
) -> Result<schema::ExecutionResultResponse, Error> {
    let clock = &requester.clock();
    let timing = timing.into();
    if size == 0 {
        return Err(Error::Execution("app is empty".into()));
    }

    // Allocate the task, whose ID addresses both the upload and the run
    progress(ExecutionProgress::Preparing);
    let taskid = requester
        .request(
            schema::ExecutionUploadStartRequest { bytes: size },
            timing.io(clock),
        )?
        .wait::<schema::ExecutionUploadStartResponse>()?
        .taskid;

    // Run the task's steps as one outcome, so any failure can cancel it
    let result = (|| {
        progress(ExecutionProgress::Started { taskid });
        progress(ExecutionProgress::Uploading {
            uploaded: 0,
            total: size,
        });

        // Stream the app, checking the source ends at its declared size
        let mut sent = 0;
        let mut uploaded = 0;
        let mut pending: Option<(Promise<Message>, u64)> = None;
        while sent < size {
            timing.check(clock)?;
            let bytes = (size - sent).min(CHUNK_SIZE as u64) as usize;
            let mut chunk = vec![0; bytes];
            reader.read_exact(&mut chunk).map_err(read_error)?;
            timing.check(clock)?;
            sent += bytes as u64;
            if sent == size {
                finish_read(reader, clock, timing)?;
            }
            // Submit the next chunk before waiting for the previous one,
            // keeping at most two outstanding while device writes overlap
            // transport I/O
            let next = requester.request(
                schema::ExecutionUploadChunkRequest { taskid, chunk },
                timing.io(clock),
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

        // Schedule the run, which waits for the owner's approval
        progress(ExecutionProgress::Authorizing);
        schedule(taskid)?;

        // Retrieving a completed status consumes the result on the Ark. This
        // workflow is the sole poller and never retries a completed retrieval.
        let started = clock.now();
        loop {
            progress(ExecutionProgress::Running {
                elapsed: clock.elapsed(started),
            });
            let status = requester
                .request(schema::ExecutionStatusRequest { taskid }, timing.io(clock))?
                .wait::<schema::ExecutionStatusResponse>()?;
            match (status.pending, status.result) {
                (false, Some(result)) => return Ok(result),
                (true, None) => {}
                _ => return Err(Error::Execution("invalid execution status".into())),
            }
            timing.pause(clock, POLL_INTERVAL)?;
        }
    })();

    // Request a failed task's cancellation within the remaining deadline, at
    // most 1 s, ignoring the outcome
    if result.is_err() {
        let cleanup = timing.io(clock).min(clock.now() + Duration::from_secs(1));
        let _ = requester
            .request(schema::ExecutionCancelRequest { taskid }, cleanup)
            .and_then(|pending| pending.wait::<schema::ExecutionCancelResponse>());
    }
    result
}

/// Checks EOF before the final chunk, so a growing or misdeclared source never
/// reaches scheduling.
///
/// Interrupted reads do not count as the end of the file.
fn finish_read(reader: &mut impl Read, clock: &Clock, timing: Timing) -> Result<(), Error> {
    loop {
        timing.check(clock)?;
        match reader.read(&mut [0]) {
            Ok(0) => break,
            Ok(_) => return Err(Error::Execution("app exceeds its advertised size".into())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_error(error)),
        }
    }
    timing.check(clock)?;
    Ok(())
}

/// Separates an expired source read from other local app read failures.
fn read_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::TimedOut {
        Error::Timeout
    } else {
        Error::ExecutionRead(error)
    }
}

/// App execution regressions against a scripted Ark.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrustMode;
    use crate::testing::{Peer, test_clock, wait_deadline};
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
        /// Declared app length the upload started with.
        size: u64,
        /// App bytes the upload delivered.
        bytes: Vec<u8>,
    }

    /// Builds an execution result with binary output, successful or not.
    fn result(success: bool) -> schema::ExecutionResultResponse {
        schema::ExecutionResultResponse {
            app_name: "test app".into(),
            app_version: "1.2.3".into(),
            success,
            stdout: vec![0, 255, 42],
            stderr: vec![254, 0],
        }
    }

    /// Spawns a peer recording the wire exchange, optionally refusing a stage.
    ///
    /// A refusing peer refuses the cancel too. Status polls take their answers
    /// from `reports`.
    fn peer(
        clock: &Clock,
        fail: Option<&'static str>,
        reports: Vec<schema::ExecutionStatusResponse>,
    ) -> (Peer, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = observed.clone();
        let mut reports = VecDeque::from(reports);
        let peer = Peer::spawn(
            clock,
            Box::new(move |session, request, responder| {
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
                let deadline = session.clock().now() + TIMEOUT;
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
            }),
        );
        (peer, observed)
    }

    /// Connects a raw wire session to the peer, pinning its identity key.
    fn attach(peer: &mut Peer) -> Session {
        let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
        protocol::connect(peer.stream(), &trust).unwrap().0
    }

    /// Runs an app under a fresh budget, scheduling it with a raw request.
    fn run(
        requester: &Requester,
        size: u64,
        reader: &mut impl Read,
        progress: impl FnMut(ExecutionProgress),
    ) -> Result<schema::ExecutionResultResponse, Error> {
        let deadline = requester.clock().now() + TIMEOUT;
        execute(requester, size, reader, deadline, progress, |taskid| {
            requester
                .request(schema::ExecutionScheduleRequest { taskid }, deadline)?
                .wait::<schema::ExecutionScheduleResponse>()?;
            Ok(())
        })
    }

    /// Scheduling follows the final acknowledgment, and polling stops at the
    /// result, keeping binary output even of a failed app.
    #[test]
    fn test_execution() {
        let mut tester = test_clock();
        let clock = tester.clock();
        for (size, success) in [(17, false), (3 * CHUNK_SIZE + 29, true)] {
            // Run the app, the first status reporting it still pending
            let expected = result(success);
            let (mut peer, observed) = peer(
                &clock,
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
            let running = thread::spawn({
                let requester = session.requester();
                let bytes = bytes.clone();
                move || {
                    let mut progress = Vec::new();
                    let result = run(&requester, size as u64, &mut bytes.as_slice(), |stage| {
                        progress.push(stage)
                    });
                    result.map(|result| (result, progress))
                }
            });

            // End the pause between the two status polls once the runner
            // sleeps in it
            let poll = clock.now() + POLL_INTERVAL;
            wait_deadline(&tester, poll);
            tester.advance_to(poll);
            let (actual, progress) = running.join().unwrap().unwrap();
            assert_eq!(actual, expected);

            // Every byte arrived, polling stopped at the result, and approval
            // followed the final acknowledgment
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

    /// A refused stage keeps its code and message even when cleanup fails too,
    /// and a refused start attempts no cancellation.
    #[test]
    fn test_refusals() {
        // Each refused stage fails the run without a retry, requesting
        // cancellation of any task it allocated
        let clock = test_clock().clock();
        for fail in ["start", "chunk", "schedule", "status"] {
            let (mut peer, observed) = peer(
                &clock,
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
        let clock = test_clock().clock();
        for report in [
            schema::ExecutionStatusResponse::default(),
            schema::ExecutionStatusResponse {
                pending: true,
                result: Some(result(true)),
            },
        ] {
            let (mut peer, observed) = peer(&clock, None, vec![report]);
            let session = attach(&mut peer);
            assert!(matches!(
                run(&session.requester(), 1, &mut [42].as_slice(), |_| {}),
                Err(Error::Execution(_))
            ));
            assert_eq!(observed.lock().unwrap().stages.last(), Some(&"cancel"));
        }
    }

    /// A source shorter or longer than declared fails before scheduling and
    /// cancels the allocated task.
    #[test]
    fn test_source_length() {
        let clock = test_clock().clock();
        for size in [17, CHUNK_SIZE + 17] {
            for extra in [-1_i64, 1] {
                let (mut peer, observed) = peer(&clock, None, vec![]);
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

    /// Two chunks can be in flight with their acknowledgments reversed, and
    /// approval waits for the last one.
    #[test]
    fn test_upload_window() {
        // The peer answers the second chunk before the first, and holds the
        // last one until the test releases it
        let clock = test_clock().clock();
        let (notice, notices) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut first = None;
        let mut chunks = 0;
        let mut peer = Peer::spawn(
            &clock,
            Box::new(move |session, request, responder| {
                let deadline = session.clock().now() + TIMEOUT;
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
                                released.recv().unwrap();
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
            }),
        );

        // Run a three chunk app, reporting its progress to the test
        let session = attach(&mut peer);
        let requester = session.requester();
        let (updates, progress) = mpsc::channel();
        let worker = thread::spawn(move || {
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

        // Approval waits for the last chunk's acknowledgment
        notices.recv().unwrap();
        assert!(
            !progress
                .try_iter()
                .any(|stage| stage == ExecutionProgress::Authorizing)
        );
        release.send(()).unwrap();
        assert!(worker.join().unwrap().unwrap().success);
    }

    /// A cancel goes out while a status poll is outstanding, with no receive
    /// loop or second connection.
    #[test]
    fn test_cancellation() {
        // The peer holds the status poll until the cancel, then answers both
        let clock = test_clock().clock();
        let (notice, notices) = mpsc::channel();
        let mut held = None;
        let mut peer = Peer::spawn(
            &clock,
            Box::new(move |session, request, responder| {
                let deadline = session.clock().now() + TIMEOUT;
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
            }),
        );

        // Cancel from another handle while the runner waits on its status poll
        let session = attach(&mut peer);
        let requester = session.requester();
        let worker = thread::spawn(move || run(&requester, 1, &mut [42].as_slice(), |_| {}));
        notices.recv().unwrap();
        session
            .requester()
            .request(
                schema::ExecutionCancelRequest { taskid: 7 },
                clock.now() + TIMEOUT,
            )
            .unwrap()
            .wait::<schema::ExecutionCancelResponse>()
            .unwrap();
        assert!(!worker.join().unwrap().unwrap().success);
    }

    /// Fragmented and interrupted reads still run the app, while an expired
    /// deadline allocates no task and a reader timeout stays a timeout.
    #[test]
    fn test_reads_and_deadlines() {
        /// Source serving one byte per read, interrupting the first read past
        /// its end.
        struct Fragmented {
            /// Bytes left to serve.
            bytes: &'static [u8],
            /// Flag set once the read past the end was interrupted.
            interrupted: bool,
        }
        impl Read for Fragmented {
            /// Serves one byte per call, interrupting once at the end of the
            /// source.
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.bytes.is_empty() && !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(&mut buffer[..1])
            }
        }

        // Fragmented and interrupted reads still run the app
        let clock = test_clock().clock();
        let (mut peer, _) = peer(
            &clock,
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

        // An expired deadline allocates no task
        let result = execute(
            &session.requester(),
            1,
            &mut [42].as_slice(),
            clock.now(),
            |_| {},
            |_| panic!("expired execution scheduled"),
        );
        assert!(matches!(result, Err(Error::Timeout)));

        // A reader timeout stays a timeout
        /// Source whose every read times out.
        struct TimedOut;
        impl Read for TimedOut {
            /// Fails every read with `TimedOut`.
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
