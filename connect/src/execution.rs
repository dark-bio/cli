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

/// Largest transfer chunk, 32 KiB short of a 2 MiB frame to leave room for
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
    /// The Ark is awaiting the owner's review of the report.
    ///
    /// Reported once, when the first awaiting status arrives.
    Reviewing,
}

/// Released result, complete output streams and time spent running the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionOutcome {
    /// Result metadata the owner saw when releasing the report.
    pub result: schema::ExecutionResultResponse,
    /// Released standard output bytes.
    pub stdout: Vec<u8>,
    /// Released standard error bytes, kept for develop builds.
    pub stderr: Vec<u8>,
    /// Time from scheduling acknowledgment to the first observed end of the run.
    pub duration: Duration,
}

/// Uploads and runs an app, streaming at most two outstanding chunks, waiting
/// for authorization and report review, then reading the released output.
///
/// Scheduling runs through the caller's client, which waits on the owner's
/// approval. After a failure it requests the task's cancellation within the
/// remaining deadline, at most 1 s, ignoring cancellation errors. Deadlines and
/// the running time are measured on the clock of the requester's session.
pub(crate) fn execute(
    requester: &Requester,
    size: u64,
    reader: &mut impl Read,
    timing: impl Into<Timing>,
    mut progress: impl FnMut(ExecutionProgress),
    schedule: impl FnOnce(u64) -> Result<(), Error>,
) -> Result<ExecutionOutcome, Error> {
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

        // Poll through the run and review, recording when the run first ends
        let started = clock.now();
        let mut duration = None;
        loop {
            let status = requester
                .request(schema::ExecutionStatusRequest { taskid }, timing.io(clock))?
                .wait::<schema::ExecutionStatusResponse>()?;
            match schema::ExecutionState::try_from(status.state) {
                Ok(schema::ExecutionState::Running) => {
                    if duration.is_none() {
                        progress(ExecutionProgress::Running {
                            elapsed: clock.elapsed(started),
                        });
                    }
                }
                Ok(schema::ExecutionState::Awaiting) => {
                    if duration.is_none() {
                        duration = Some(clock.elapsed(started));
                        progress(ExecutionProgress::Reviewing);
                    }
                }
                Ok(schema::ExecutionState::Resolved) => {
                    duration.get_or_insert_with(|| clock.elapsed(started));
                    break;
                }
                _ => return Err(Error::Execution("invalid execution status".into())),
            }
            timing.pause(clock, POLL_INTERVAL)?;
        }

        // Fetch the released result, then read each output stream in order
        let result = requester
            .request(schema::ExecutionResultRequest { taskid }, timing.io(clock))?
            .wait::<schema::ExecutionResultResponse>()?;
        let stdout = read_output(
            requester,
            taskid,
            schema::ExecutionStream::Stdout,
            result.stdout_bytes,
            timing,
        )?;
        let stderr = read_output(
            requester,
            taskid,
            schema::ExecutionStream::Stderr,
            result.stderr_bytes,
            timing,
        )?;
        Ok(ExecutionOutcome {
            result,
            stdout,
            stderr,
            duration: duration.expect("resolved execution ended"),
        })
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

/// Reads one released stream to its advertised length, rejecting invalid chunks.
fn read_output(
    requester: &Requester,
    taskid: u64,
    stream: schema::ExecutionStream,
    size: u64,
    timing: Timing,
) -> Result<Vec<u8>, Error> {
    let clock = requester.clock();
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < size {
        let response = requester
            .request(
                schema::ExecutionOutputRequest {
                    taskid,
                    stream: stream as i32,
                    offset,
                    size: CHUNK_SIZE as u64,
                },
                timing.io(&clock),
            )?
            .wait::<schema::ExecutionOutputResponse>()?;
        if response.chunk.len() as u64 != (size - offset).min(CHUNK_SIZE as u64) {
            return Err(Error::Execution("invalid execution output chunk".into()));
        }
        offset += response.chunk.len() as u64;
        output.extend(response.chunk);
    }
    Ok(output)
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
        /// Output reads, with their requested streams, sizes and offsets.
        reads: Vec<schema::ExecutionOutputRequest>,
    }

    /// Builds released result metadata with empty streams, successful or not.
    fn result(success: bool) -> schema::ExecutionResultResponse {
        schema::ExecutionResultResponse {
            name: "sample app".into(),
            version: "1.2.3".into(),
            develop: true,
            success,
            paths: vec!["v1/sample".into()],
            media: "text/plain".into(),
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }

    /// Builds a released outcome with binary output on both streams.
    fn outcome(success: bool) -> ExecutionOutcome {
        ExecutionOutcome {
            result: schema::ExecutionResultResponse {
                stdout_bytes: 3,
                stderr_bytes: 2,
                ..result(success)
            },
            stdout: vec![0, 255, 42],
            stderr: vec![254, 0],
            duration: Duration::ZERO,
        }
    }

    /// Spawns a peer recording the wire exchange, optionally refusing a stage.
    ///
    /// A refusing peer refuses the cancel too. Status polls take their answers
    /// from `reports`.
    fn peer(
        clock: &Clock,
        fail: Option<(&'static str, schema::Error)>,
        reports: Vec<i32>,
        output: ExecutionOutcome,
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
                            schema::ExecutionStatusResponse {
                                state: reports.pop_front().expect("unexpected status poll"),
                            }
                            .into(),
                        )
                    }
                    Content::ExecResult(request) => {
                        assert_eq!(request.taskid, 7);
                        ("result", output.result.clone().into())
                    }
                    Content::ExecOutput(request) => {
                        assert_eq!(request.taskid, 7);
                        let bytes = match schema::ExecutionStream::try_from(request.stream) {
                            Ok(schema::ExecutionStream::Stdout) => &output.stdout,
                            Ok(schema::ExecutionStream::Stderr) => &output.stderr,
                            _ => panic!("invalid output stream"),
                        };
                        let chunk = bytes
                            .get(request.offset as usize..)
                            .unwrap_or_default()
                            .iter()
                            .copied()
                            .take(request.size as usize)
                            .collect();
                        observed.reads.push(request);
                        ("output", schema::ExecutionOutputResponse { chunk }.into())
                    }
                    Content::ExecCancel(request) => {
                        assert_eq!(request.taskid, 7);
                        ("cancel", schema::ExecutionCancelResponse {}.into())
                    }
                    _ => panic!("unexpected request"),
                };
                observed.stages.push(stage);
                let deadline = session.clock().now() + TIMEOUT;
                if let Some((failed, error)) = &fail
                    && (*failed == stage || stage == "cancel")
                {
                    let error = if stage == "cancel" {
                        schema::Error::new(0x779, "cleanup refused")
                    } else {
                        error.clone()
                    };
                    responder.fail(error, deadline).unwrap();
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
    ) -> Result<ExecutionOutcome, Error> {
        let deadline = requester.clock().now() + TIMEOUT;
        execute(requester, size, reader, deadline, progress, |taskid| {
            requester
                .request(schema::ExecutionScheduleRequest { taskid }, deadline)?
                .wait::<schema::ExecutionScheduleResponse>()?;
            Ok(())
        })
    }

    /// Resolution ends polling, output reads follow the release, and the run's
    /// duration excludes the review even for a failed app.
    #[test]
    fn test_execution() {
        let mut tester = test_clock();
        let clock = tester.clock();
        for (size, success) in [(17, false), (3 * CHUNK_SIZE + 29, true)] {
            // Serve a run and repeated review polls before releasing both streams
            let mut expected = outcome(success);
            expected.stdout = (0..2 * CHUNK_SIZE + 29).map(|i| (i % 251) as u8).collect();
            expected.result.stdout_bytes = expected.stdout.len() as u64;
            expected.duration = Duration::from_millis(500);
            let (mut peer, observed) = peer(
                &clock,
                None,
                vec![
                    schema::ExecutionState::Running as i32,
                    schema::ExecutionState::Awaiting as i32,
                    schema::ExecutionState::Awaiting as i32,
                    schema::ExecutionState::Resolved as i32,
                ],
                expected.clone(),
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

            // Advance each polling pause, spending a full second in review
            let started = clock.now();
            for millis in [500, 1000, 1500] {
                let poll = started + Duration::from_millis(millis);
                wait_deadline(&tester, poll);
                tester.advance_to(poll);
            }
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
                4
            );
            assert!(!observed.stages.contains(&"cancel"));
            assert_eq!(
                &observed.stages[observed.stages.len() - 6..],
                ["status", "result", "output", "output", "output", "output"]
            );
            assert_eq!(
                observed
                    .reads
                    .iter()
                    .map(|read| (read.stream, read.offset, read.size))
                    .collect::<Vec<_>>(),
                [
                    (1, 0, 2_064_384),
                    (1, 2_064_384, 2_064_384),
                    (1, 4_128_768, 2_064_384),
                    (2, 0, 2_064_384),
                ]
            );
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
            assert_eq!(
                &progress[authorizing + 1..],
                [
                    ExecutionProgress::Running {
                        elapsed: Duration::ZERO,
                    },
                    ExecutionProgress::Reviewing,
                ]
            );
        }
    }

    /// A refused stage keeps its code and message even when cleanup fails too,
    /// and a refused start attempts no cancellation.
    #[test]
    fn test_refusals() {
        // Each refused stage fails the run without a retry, requesting
        // cancellation of any task it allocated
        let clock = test_clock().clock();
        for fail in ["start", "chunk", "schedule", "status", "result", "output"] {
            let (mut peer, observed) = peer(
                &clock,
                Some((fail, schema::Error::new(0x778, format!("refused {fail}")))),
                vec![schema::ExecutionState::Resolved as i32],
                outcome(true),
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

    /// Unspecified and unknown states fail and cancel the task.
    #[test]
    fn test_invalid_status() {
        let clock = test_clock().clock();
        for report in [schema::ExecutionState::Unspecified as i32, 99] {
            let (mut peer, observed) = peer(&clock, None, vec![report], outcome(true));
            let session = attach(&mut peer);
            assert!(matches!(
                run(&session.requester(), 1, &mut [42].as_slice(), |_| {}),
                Err(Error::Execution(message)) if message == "invalid execution status"
            ));
            assert_eq!(observed.lock().unwrap().stages.last(), Some(&"cancel"));
        }
    }

    /// A resolved observation ends the run without prompting for an unseen review.
    #[test]
    fn test_resolved_without_awaiting() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut peer, observed) = peer(
            &clock,
            None,
            vec![
                schema::ExecutionState::Running as i32,
                schema::ExecutionState::Resolved as i32,
            ],
            outcome(true),
        );
        let session = attach(&mut peer);
        let worker = thread::spawn({
            let requester = session.requester();
            move || {
                let mut progress = Vec::new();
                let outcome = run(&requester, 1, &mut [42].as_slice(), |stage| {
                    progress.push(stage)
                })
                .unwrap();
                (outcome, progress)
            }
        });

        // Finish the run at the second poll, without a separate review wait
        let poll = clock.now() + Duration::from_millis(500);
        wait_deadline(&tester, poll);
        tester.advance_to(poll);
        let (outcome, progress) = worker.join().unwrap();
        assert_eq!(outcome.duration, Duration::from_millis(500));
        assert!(!progress.contains(&ExecutionProgress::Reviewing));
        assert_eq!(
            observed.lock().unwrap().stages,
            [
                "start", "chunk", "schedule", "status", "status", "result", "output", "output"
            ]
        );
    }

    /// Timely status replies keep a review alive beyond the inactivity allowance.
    #[test]
    fn test_review_outlasts_inactivity() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut states = vec![schema::ExecutionState::Awaiting as i32; 24];
        states.push(schema::ExecutionState::Resolved as i32);
        let (mut peer, _) = peer(&clock, None, states, outcome(true));
        let session = attach(&mut peer);
        let worker = thread::spawn({
            let requester = session.requester();
            move || {
                let timing = Timing::inactivity(Duration::from_millis(100));
                let mut progress = Vec::new();
                let outcome = execute(
                    &requester,
                    1,
                    &mut [42].as_slice(),
                    timing,
                    |stage| progress.push(stage),
                    |taskid| {
                        requester
                            .request(
                                schema::ExecutionScheduleRequest { taskid },
                                timing.io(&requester.clock()),
                            )?
                            .wait::<schema::ExecutionScheduleResponse>()?;
                        Ok(())
                    },
                )
                .unwrap();
                (outcome, progress)
            }
        });

        // Each reply renews the I/O allowance while review has no total bound
        let started = clock.now();
        for index in 1..=24 {
            let poll = started + Duration::from_millis(index * 500);
            wait_deadline(&tester, poll);
            tester.advance_to(poll);
        }
        let (outcome, progress) = worker.join().unwrap();
        assert_eq!(outcome.duration, Duration::ZERO);
        assert_eq!(
            &progress[progress.len() - 2..],
            [ExecutionProgress::Authorizing, ExecutionProgress::Reviewing]
        );
    }

    /// Every withheld report keeps its reserved code and message.
    #[test]
    fn test_withheld_reports() {
        let clock = test_clock().clock();
        for (code, message) in [
            (
                schema::ReservedErrors::Unauthorized,
                "sample report declined",
            ),
            (schema::ReservedErrors::Unconfirmed, "sample review expired"),
            (
                schema::ReservedErrors::Undelivered,
                "sample review not delivered",
            ),
        ] {
            let expected = schema::Error::reserved(code, message);
            let (mut peer, observed) = peer(
                &clock,
                Some(("result", expected.clone())),
                vec![schema::ExecutionState::Resolved as i32],
                outcome(true),
            );
            let session = attach(&mut peer);
            assert!(matches!(
                run(&session.requester(), 1, &mut [42].as_slice(), |_| {}),
                Err(Error::Remote(error)) if error == expected
            ));
            assert_eq!(
                observed.lock().unwrap().stages,
                ["start", "chunk", "schedule", "status", "result", "cancel"]
            );
        }
    }

    /// Empty, short and overrunning chunks fail and cancel either output stream.
    #[test]
    fn test_invalid_output_chunks() {
        let clock = test_clock().clock();
        for stream in [
            schema::ExecutionStream::Stdout,
            schema::ExecutionStream::Stderr,
        ] {
            for length in [0, 2, 4] {
                let mut output = outcome(true);
                if stream == schema::ExecutionStream::Stdout {
                    output.stdout = vec![42; length];
                } else {
                    output.result.stderr_bytes = 3;
                    output.stderr = vec![42; length];
                }
                let (mut peer, observed) = peer(
                    &clock,
                    None,
                    vec![schema::ExecutionState::Resolved as i32],
                    output,
                );
                let session = attach(&mut peer);
                assert!(matches!(
                    run(&session.requester(), 1, &mut [42].as_slice(), |_| {}),
                    Err(Error::Execution(_))
                ));
                let observed = observed.lock().unwrap();
                assert_eq!(observed.stages.last(), Some(&"cancel"));
                assert_eq!(observed.reads.last().unwrap().stream, stream as i32);
            }
        }
    }

    /// Empty streams issue no output read, including when only one is empty.
    #[test]
    fn test_empty_streams() {
        let clock = test_clock().clock();
        for (stdout, stderr, streams) in [
            (false, false, vec![]),
            (false, true, vec![2]),
            (true, false, vec![1]),
        ] {
            let mut expected = outcome(true);
            if !stdout {
                expected.stdout.clear();
                expected.result.stdout_bytes = 0;
            }
            if !stderr {
                expected.stderr.clear();
                expected.result.stderr_bytes = 0;
            }
            let (mut peer, observed) = peer(
                &clock,
                None,
                vec![schema::ExecutionState::Resolved as i32],
                expected.clone(),
            );
            let session = attach(&mut peer);
            let mut progress = Vec::new();
            let actual = run(&session.requester(), 1, &mut [42].as_slice(), |stage| {
                progress.push(stage)
            })
            .unwrap();
            assert_eq!(actual, expected);
            assert!(!progress.contains(&ExecutionProgress::Reviewing));
            let observed = observed.lock().unwrap();
            assert_eq!(
                observed
                    .reads
                    .iter()
                    .map(|read| read.stream)
                    .collect::<Vec<_>>(),
                streams
            );
            assert!(!observed.stages.contains(&"cancel"));
        }
    }

    /// A source shorter or longer than declared fails before scheduling and
    /// cancels the allocated task.
    #[test]
    fn test_source_length() {
        let clock = test_clock().clock();
        for size in [17, CHUNK_SIZE + 17] {
            for extra in [-1_i64, 1] {
                let (mut peer, observed) = peer(&clock, None, vec![], outcome(true));
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
                                    state: schema::ExecutionState::Resolved as i32,
                                },
                                deadline,
                            )
                            .unwrap();
                    }
                    Content::ExecResult(_) => {
                        responder.reply(result(true), deadline).unwrap();
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
        assert!(worker.join().unwrap().unwrap().result.success);
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
                                    state: schema::ExecutionState::Resolved as i32,
                                },
                                deadline,
                            )
                            .unwrap();
                    }
                    Content::ExecResult(_) => {
                        responder.reply(result(false), deadline).unwrap();
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
        assert!(!worker.join().unwrap().unwrap().result.success);
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
            vec![schema::ExecutionState::Resolved as i32],
            outcome(true),
        );
        let session = attach(&mut peer);
        let mut reader = Fragmented {
            bytes: &[1, 2, 3],
            interrupted: false,
        };
        assert!(
            run(&session.requester(), 3, &mut reader, |_| {})
                .unwrap()
                .result
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
