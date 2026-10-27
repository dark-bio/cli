// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Runs an app on the Ark, from serving its module to reading its released
//! result.

use crate::incoming::Incoming;
use crate::{Error, Timing, schema};
use darkbio_wire::protocol::{Message, Requester};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

/// Largest output chunk, 32 KiB short of a 2 MiB frame to leave room for
/// sealing and framing.
const CHUNK_SIZE: usize = 2 * 1024 * 1024 - 32 * 1024;
/// Largest module read allowed by the wire.
const MODULE_READ_SIZE: u64 = 1024 * 1024;
/// Delay between status requests while the Ark retains a pending task.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Execution stages reported on the caller's thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionProgress {
    /// Hashing the app and creating its task on the Ark.
    Preparing,
    /// The Ark allocated this task; cancellation can now address it.
    Started {
        /// Task ID accepted by [`schema::ExecutionCancelRequest`].
        taskid: u64,
    },
    /// Loading, while the Ark fetches the app's module and checks it.
    Loading,
    /// Waiting for the owner's approval of the loaded app.
    Authorizing,
    /// Running, with the time elapsed since the Ark acknowledged the start.
    Running {
        /// Host time since the start succeeded, status polls included.
        elapsed: Duration,
    },
    /// The Ark is awaiting the owner's review of the report.
    ///
    /// Reported once, when the first awaiting status arrives.
    Reviewing {
        /// Host time from the start to the first status awaiting review.
        elapsed: Duration,
    },
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
    /// Time from the acknowledged start to the first observed end of the run.
    pub duration: Duration,
}

/// Creates, loads and starts an app, then reads its released output.
///
/// Starting waits for the owner's approval. On a failure it tries to cancel
/// the task within the time left, at most 1s, and ignores a failed cancel.
/// Deadlines and running time use the clock of the requester's session.
pub(crate) fn execute(
    requester: &Requester,
    incoming: &Arc<Incoming>,
    reader: &mut (impl Read + Seek),
    timing: Timing,
    mut progress: impl FnMut(ExecutionProgress),
) -> Result<ExecutionOutcome, Error> {
    let clock = &requester.clock();

    // Hash the source before creating a task that the Ark may expire
    progress(ExecutionProgress::Preparing);
    let (sha256, size) = hash(reader)?;
    timing.check(clock)?;
    let taskid = requester
        .request(
            schema::ExecutionCreateRequest {
                sha256: sha256.to_vec(),
            },
            timing.io(clock),
        )?
        .wait::<schema::ExecutionCreateResponse>()?
        .taskid;

    // Run the task's steps as one outcome, so any failure can cancel it
    let result = (|| {
        progress(ExecutionProgress::Started { taskid });
        progress(ExecutionProgress::Loading);

        // Serve the source while the Ark loads and checks the app
        let mut loading = requester.request(
            schema::ExecutionLoadRequest { taskid },
            // The wire needs an instant even though loading has no time limit
            timing.limit(clock.now() + Duration::from_secs(100 * 365 * 24 * 60 * 60)),
        )?;
        incoming.serve(&mut loading, |request, responder| {
            let mut dependency = false;
            let response: Result<Message, _> = match request {
                schema::ark_to_host::Content::ExecModuleSize(request) => {
                    if request.taskid != taskid {
                        Err((schema::ReservedErrors::Unspecified, "unknown task"))
                    } else if request.sha256 != sha256 {
                        dependency = true;
                        Err((schema::ReservedErrors::Unspecified, "module not available"))
                    } else {
                        Ok(schema::ExecutionModuleSizeResponse { size }.into())
                    }
                }
                schema::ark_to_host::Content::ExecModuleRead(request) => {
                    if request.taskid != taskid {
                        Err((schema::ReservedErrors::Unspecified, "unknown task"))
                    } else if request.sha256 != sha256 {
                        Err((schema::ReservedErrors::Unspecified, "module not available"))
                    } else if request.size == 0
                        || request.size > MODULE_READ_SIZE
                        || request.offset >= size
                        || request.size > size - request.offset
                    {
                        Err((schema::ReservedErrors::Unspecified, "invalid module range"))
                    } else {
                        reader
                            .seek(SeekFrom::Start(request.offset))
                            .map_err(read_error)?;
                        let mut chunk = vec![0; request.size as usize];
                        reader.read_exact(&mut chunk).map_err(read_error)?;
                        Ok(schema::ExecutionModuleReadResponse { chunk }.into())
                    }
                }
                _ => Err((schema::ReservedErrors::Unsupported, "request not supported")),
            };

            // Give the reply a fresh allowance after any local read
            let deadline = timing.io(clock);
            match response {
                Ok(response) => responder.reply(response, deadline),
                Err((code, message)) => {
                    responder.fail(schema::Error::reserved(code, message), deadline)
                }
            }?
            .wait()?;
            if dependency {
                return Err(Error::ExecutionDependency);
            }
            Ok(())
        })?;
        loading.wait::<schema::ExecutionLoadResponse>()?;

        // Start the run, which waits for the owner's approval
        progress(ExecutionProgress::Authorizing);
        requester
            .request(
                schema::ExecutionStartRequest { taskid },
                timing.approval(clock),
            )?
            .wait::<schema::ExecutionStartResponse>()?;

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
                        let elapsed = clock.elapsed(started);
                        duration = Some(elapsed);
                        progress(ExecutionProgress::Reviewing { elapsed });
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
    // most 1s, ignoring the outcome
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
        let requested = CHUNK_SIZE as u64;
        let response = requester
            .request(
                schema::ExecutionOutputRequest {
                    taskid,
                    stream: stream as i32,
                    offset,
                    size: requested,
                },
                timing.io(&clock),
            )?
            .wait::<schema::ExecutionOutputResponse>()?;
        if response.chunk.len() as u64 != (size - offset).min(requested) {
            return Err(Error::Execution("invalid execution output chunk".into()));
        }
        offset += response.chunk.len() as u64;
        output.extend(response.chunk);
    }
    Ok(output)
}

/// Hashes the whole source with bounded memory and measures its length.
fn hash(reader: &mut (impl Read + Seek)) -> Result<([u8; 32], u64), Error> {
    reader.rewind().map_err(read_error)?;
    let mut digest = Sha256::new();
    let size = io::copy(reader, &mut digest).map_err(read_error)?;
    if size == 0 {
        return Err(Error::Execution("app is empty".into()));
    }
    Ok((digest.finalize().into(), size))
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
    use crate::testing::{Peer, Script, test_clock, wait_deadline};
    use crate::{Ark, TrustMode};
    use darkbio_clock::Clock;
    use darkbio_wire::protocol::{self, Session};
    use schema::host_to_ark::Content;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::{Mutex, mpsc};
    use std::thread;

    /// Budget for test I/O that is not exercising expiration.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Wire exchange recorded by the scripted peer.
    #[derive(Default)]
    struct Observed {
        /// Stages the peer served, in arrival order.
        stages: Vec<&'static str>,
        /// Digest that created the task.
        digest: Vec<u8>,
        /// Source length returned by the host.
        size: u64,
        /// Module bytes returned by the host.
        bytes: Vec<u8>,
        /// Length of each module read response.
        chunks: Vec<usize>,
        /// Output reads, with their requested streams, sizes and offsets.
        reads: Vec<schema::ExecutionOutputRequest>,
    }

    /// Builds a released outcome with binary output on both streams.
    fn outcome(success: bool) -> ExecutionOutcome {
        ExecutionOutcome {
            result: schema::ExecutionResultResponse {
                name: "sample app".into(),
                version: "1.2.3".into(),
                develop: true,
                success,
                paths: vec!["v1/sample".into()],
                media: "text/markdown".into(),
                stdout_bytes: 3,
                stderr_bytes: 2,
            },
            stdout: vec![0, 255, 42],
            stderr: vec![254, 0],
            duration: Duration::ZERO,
        }
    }

    /// Fetches the module by digest, continuing at the end of each answer.
    fn pull(session: &Session, observed: &mut Observed) {
        let requester = session.requester();
        let size = requester
            .request(
                schema::ExecutionModuleSizeRequest {
                    taskid: 7,
                    sha256: observed.digest.clone(),
                },
                session.clock().now() + TIMEOUT,
            )
            .unwrap()
            .wait::<schema::ExecutionModuleSizeResponse>()
            .unwrap();
        observed.size = size.size;
        while (observed.bytes.len() as u64) < size.size {
            let chunk = requester
                .request(
                    schema::ExecutionModuleReadRequest {
                        taskid: 7,
                        sha256: observed.digest.clone(),
                        offset: observed.bytes.len() as u64,
                        size: (size.size - observed.bytes.len() as u64).min(1_048_576),
                    },
                    session.clock().now() + TIMEOUT,
                )
                .unwrap()
                .wait::<schema::ExecutionModuleReadResponse>()
                .unwrap()
                .chunk;
            assert!(!chunk.is_empty());
            observed.chunks.push(chunk.len());
            observed.bytes.extend(chunk);
        }
    }

    /// Spawns a peer that records the flow and may refuse one stage.
    ///
    /// Its load callback sends the module requests before it acknowledges the
    /// load. A refusing peer also refuses the cancel, so the first error stays.
    fn peer(
        clock: &Clock,
        fail: Option<(&'static str, schema::Error)>,
        reports: Vec<i32>,
        output: ExecutionOutcome,
        mut load: impl FnMut(&Session, &mut Observed) + Send + 'static,
    ) -> (Peer, Arc<Mutex<Observed>>) {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = observed.clone();
        let mut reports = VecDeque::from(reports);
        let mut served = [0; 2];
        let peer = scripted(
            clock,
            Box::new(move |session, request, responder| {
                let mut observed = shared.lock().unwrap();
                let (stage, response): (_, Message) = match request {
                    Content::ExecCreate(request) => {
                        observed.digest = request.sha256;
                        (
                            "create",
                            schema::ExecutionCreateResponse { taskid: 7 }.into(),
                        )
                    }
                    Content::ExecLoad(request) => {
                        assert_eq!(request.taskid, 7);
                        load(session, &mut observed);
                        ("load", schema::ExecutionLoadResponse {}.into())
                    }
                    Content::ExecStart(request) => {
                        assert_eq!(request.taskid, 7);
                        ("start", schema::ExecutionStartResponse {}.into())
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
                        let (bytes, offset) =
                            match schema::ExecutionStream::try_from(request.stream) {
                                Ok(schema::ExecutionStream::Stdout) => {
                                    (&output.stdout, &mut served[0])
                                }
                                Ok(schema::ExecutionStream::Stderr) => {
                                    (&output.stderr, &mut served[1])
                                }
                                _ => panic!("invalid output stream"),
                            };
                        assert_eq!(request.offset, *offset);
                        let chunk: Vec<_> = bytes
                            .get(request.offset as usize..)
                            .unwrap_or_default()
                            .iter()
                            .copied()
                            .take(request.size as usize)
                            .collect();
                        *offset += chunk.len() as u64;
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

    /// Serves cloud setup before task creation, refusing any later setup check.
    fn scripted(clock: &Clock, mut script: Script) -> Peer {
        let mut created = false;
        Peer::spawn(
            clock,
            Box::new(move |session, request, responder| {
                if let Content::DeviceInfo(_) = request {
                    assert!(!created, "setup checked after task creation");
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
                            session.clock().now() + TIMEOUT,
                        )
                        .unwrap();
                    true
                } else {
                    created |= matches!(request, Content::ExecCreate(_));
                    script(session, request, responder)
                }
            }),
        )
    }

    /// Connects through the dispatcher, reusing the peer's cloud setup.
    fn attach(peer: &mut Peer) -> Ark {
        let trust = TrustMode::Recover(Box::new(peer.identity.clone()));
        Ark::attach(peer.stream(), &trust, |_| {
            Some((
                crate::trust::Environment::Develop,
                crate::trust::Realm::Hardware,
            ))
        })
        .unwrap()
        .0
    }

    /// Runs the supplied source under a fresh fixed budget.
    fn run(
        ark: &mut Ark,
        reader: &mut (impl Read + Seek),
        progress: impl FnMut(ExecutionProgress),
    ) -> Result<ExecutionOutcome, Error> {
        ark.execute(reader, ark.client().clock().now() + TIMEOUT, progress)
    }

    /// Loading precedes approval, and released output retains both streams and
    /// a duration that excludes the owner's review.
    #[test]
    fn test_execution() {
        let mut tester = test_clock();
        let clock = tester.clock();
        for (size, success) in [(17, false), (3 * 1_048_576 + 29, true)] {
            // Serve repeated review polls before releasing the output
            let mut expected = outcome(success);
            expected.stdout = (0..2 * 1_048_576 + 29).map(|i| (i % 251) as u8).collect();
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
                pull,
            );
            let mut ark = attach(&mut peer);
            let bytes: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let source = bytes.clone();
            let running = thread::spawn(move || {
                let mut progress = Vec::new();
                let result = run(&mut ark, &mut Cursor::new(source), |stage| {
                    progress.push(stage)
                });
                result.map(|result| (result, progress))
            });

            // Advance each polling pause, spending a full second in review
            let started = clock.now();
            for millis in [500, 1000, 1500] {
                let poll = started + Duration::from_millis(millis);
                wait_deadline(&tester, poll);
                tester.advance_to(poll);
            }
            let (actual, progress) = running.join().unwrap().unwrap();
            assert_eq!(actual, expected, "{size}");
            let observed = observed.lock().unwrap();
            assert_eq!(observed.size, size as u64, "{size}");
            assert_eq!(observed.bytes, bytes, "{size}");
            assert_eq!(
                observed.stages[..8],
                [
                    "create", "load", "start", "status", "status", "status", "status", "result"
                ],
                "{size}"
            );
            assert!(
                observed.stages[8..].iter().all(|stage| *stage == "output"),
                "{size}"
            );
            assert_eq!(
                observed.chunks,
                if size == 17 {
                    vec![17]
                } else {
                    vec![1_048_576, 1_048_576, 1_048_576, 29]
                },
                "{size}"
            );
            assert_eq!(
                progress,
                [
                    ExecutionProgress::Preparing,
                    ExecutionProgress::Started { taskid: 7 },
                    ExecutionProgress::Loading,
                    ExecutionProgress::Authorizing,
                    ExecutionProgress::Running {
                        elapsed: Duration::ZERO
                    },
                    ExecutionProgress::Reviewing {
                        elapsed: Duration::from_millis(500)
                    }
                ],
                "{size}"
            );
        }
    }

    /// A size request for another digest cancels the task before approval.
    #[test]
    fn test_dependency_refusal() {
        let clock = test_clock().clock();
        let (mut peer, observed) = peer(&clock, None, vec![], outcome(true), |session, _| {
            let response = session
                .requester()
                .request(
                    schema::ExecutionModuleSizeRequest {
                        taskid: 7,
                        sha256: vec![0; 32],
                    },
                    session.clock().now() + TIMEOUT,
                )
                .unwrap()
                .wait::<Message>();
            assert!(matches!(response, Err(protocol::Error::Remote(error))
                    if error.code == 0 && !error.msg.is_empty()));
        });
        let mut ark = attach(&mut peer);
        let error = run(&mut ark, &mut Cursor::new(b"abc"), |_| {}).unwrap_err();
        assert!(matches!(error, Error::ExecutionDependency));
        let observed = observed.lock().unwrap();
        assert_eq!(observed.stages, ["create", "load", "cancel"]);

        // The standard SHA-256 "abc" vector pins the created task's digest
        assert_eq!(
            hex::encode(&observed.digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Bad ranges and other tasks get no bytes, and valid reads still work.
    #[test]
    fn test_module_request_validation() {
        let clock = test_clock().clock();
        let (mut peer, _) = peer(
            &clock,
            None,
            vec![schema::ExecutionState::Resolved as i32],
            outcome(true),
            |session, observed| {
                for (i, (taskid, offset, size)) in [
                    (8, 0, 1),         // another task
                    (7, 0, 0),         // empty read
                    (7, 0, 1_048_577), // over the wire cap, within the source
                    (7, u64::MAX, 1),  // offset that would overflow an end
                    (7, 1_048_577, 2), // read past the source's end
                ]
                .into_iter()
                .enumerate()
                {
                    let response = session
                        .requester()
                        .request(
                            schema::ExecutionModuleReadRequest {
                                taskid,
                                sha256: observed.digest.clone(),
                                offset,
                                size,
                            },
                            session.clock().now() + TIMEOUT,
                        )
                        .unwrap()
                        .wait::<Message>();
                    assert!(
                        matches!(response, Err(protocol::Error::Remote(error)) if error.code == 0),
                        "test {i}"
                    );
                }

                // A valid read still returns the last byte after the refusals
                let response = session
                    .requester()
                    .request(
                        schema::ExecutionModuleReadRequest {
                            taskid: 7,
                            sha256: observed.digest.clone(),
                            offset: 1_048_577,
                            size: 1,
                        },
                        session.clock().now() + TIMEOUT,
                    )
                    .unwrap()
                    .wait::<schema::ExecutionModuleReadResponse>()
                    .unwrap();
                assert_eq!(response.chunk, [42]);
            },
        );

        // The source holds the oversized read, so the cap alone refuses it
        let mut ark = attach(&mut peer);
        assert!(
            run(&mut ark, &mut Cursor::new(vec![42; 1_048_578]), |_| {})
                .unwrap()
                .result
                .success
        );
    }

    /// A refused stage keeps its verdict even when cleanup fails too.
    #[test]
    fn test_refusals() {
        let clock = test_clock().clock();
        for failed in ["create", "load", "start", "status", "result", "output"] {
            let expected = schema::Error::new(0x778, format!("refused {failed}"));
            let (mut peer, observed) = peer(
                &clock,
                Some((failed, expected.clone())),
                vec![schema::ExecutionState::Resolved as i32],
                outcome(true),
                |_, _| {},
            );
            let mut ark = attach(&mut peer);
            let result = run(&mut ark, &mut Cursor::new(b"abc"), |_| {});
            assert!(
                matches!(result, Err(Error::Remote(error)) if error == expected),
                "{failed}"
            );
            let observed = observed.lock().unwrap();
            assert_eq!(
                observed.stages.contains(&"cancel"),
                failed != "create",
                "{failed}"
            );
            assert_eq!(
                observed
                    .stages
                    .iter()
                    .filter(|stage| **stage == failed)
                    .count(),
                1,
                "{failed}"
            );
        }
    }

    /// Invalid execution states fail and cancel the task.
    #[test]
    fn test_invalid_status() {
        let clock = test_clock().clock();
        for report in [0, 99] {
            let (mut peer, observed) = peer(&clock, None, vec![report], outcome(true), pull);
            let mut ark = attach(&mut peer);
            assert!(
                matches!(
                    run(&mut ark, &mut Cursor::new(b"abc"), |_| {}),
                    Err(Error::Execution(_))
                ),
                "{report}"
            );
            assert_eq!(
                observed.lock().unwrap().stages.last(),
                Some(&"cancel"),
                "{report}"
            );
        }
    }

    /// A resolved observation ends the run without an unseen review prompt.
    #[test]
    fn test_resolved_without_awaiting() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut peer, _) = peer(
            &clock,
            None,
            vec![
                schema::ExecutionState::Running as i32,
                schema::ExecutionState::Resolved as i32,
            ],
            outcome(true),
            pull,
        );
        let mut ark = attach(&mut peer);
        let worker = thread::spawn(move || {
            let mut progress = Vec::new();
            let outcome = run(&mut ark, &mut Cursor::new(b"abc"), |stage| {
                progress.push(stage)
            })
            .unwrap();
            (outcome, progress)
        });
        let poll = clock.now() + Duration::from_millis(500);
        wait_deadline(&tester, poll);
        tester.advance_to(poll);
        let (outcome, progress) = worker.join().unwrap();
        assert_eq!(outcome.duration, Duration::from_millis(500));
        assert!(
            !progress
                .iter()
                .any(|stage| matches!(stage, ExecutionProgress::Reviewing { .. }))
        );
    }

    /// Review replies renew the machine allowance, with no total bound.
    #[test]
    fn test_review_outlasts_inactivity() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut states = vec![schema::ExecutionState::Awaiting as i32; 24];
        states.push(schema::ExecutionState::Resolved as i32);
        let (mut peer, _) = peer(&clock, None, states, outcome(true), pull);
        let mut ark = attach(&mut peer);
        let worker = thread::spawn(move || {
            ark.execute(
                &mut Cursor::new(b"abc"),
                Timing::inactivity(Duration::from_millis(100)),
                |_| {},
            )
        });
        let started = clock.now();
        for index in 1..=24 {
            let poll = started + Duration::from_millis(index * 500);
            wait_deadline(&tester, poll);
            tester.advance_to(poll);
        }
        assert_eq!(worker.join().unwrap().unwrap().duration, Duration::ZERO);
    }

    /// Loading outlasts the inactivity limit, and starting waits for the
    /// owner's approval.
    #[test]
    fn test_loading_and_approval_outlast_inactivity() {
        // Hold both responses until the test advances the clock for each stage
        let mut tester = test_clock();
        let clock = tester.clock();
        let (asked, requests) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let mut peer = scripted(
            &clock,
            Box::new(move |session, request, responder| {
                let response: Message = match request {
                    Content::ExecCreate(_) => schema::ExecutionCreateResponse { taskid: 7 }.into(),
                    Content::ExecLoad(_) => {
                        asked.send(()).unwrap();
                        releases.recv().unwrap();
                        schema::ExecutionLoadResponse {}.into()
                    }
                    Content::ExecStart(_) => {
                        asked.send(()).unwrap();
                        releases.recv().unwrap();
                        schema::ExecutionStartResponse {}.into()
                    }
                    Content::ExecStatus(_) => schema::ExecutionStatusResponse {
                        state: schema::ExecutionState::Resolved as i32,
                    }
                    .into(),
                    Content::ExecResult(_) => schema::ExecutionResultResponse {
                        stdout_bytes: 0,
                        stderr_bytes: 0,
                        ..outcome(true).result
                    }
                    .into(),
                    _ => panic!("unexpected request"),
                };
                responder
                    .reply(response, session.clock().now() + TIMEOUT)
                    .unwrap();
                true
            }),
        );
        let mut ark = attach(&mut peer);
        let worker = thread::spawn(move || {
            ark.execute(
                &mut Cursor::new(b"abc"),
                Timing::inactivity(Duration::from_millis(100)),
                |_| {},
            )
        });

        // Loading passes 600s, then the owner's approval takes more than 40s
        for seconds in [601, 41] {
            requests.recv().unwrap();
            tester.advance(Duration::from_secs(seconds));
            release.send(()).unwrap();
        }
        assert!(worker.join().unwrap().unwrap().result.success);
    }

    /// Disconnects and caller deadlines wake loading without a module request.
    #[test]
    fn test_loading_disconnect_and_deadline() {
        for expire in [false, true] {
            let mut tester = test_clock();
            let clock = tester.clock();
            let (loaded, loads) = mpsc::channel();
            let mut peer = scripted(
                &clock,
                Box::new(move |session, request, responder| {
                    let deadline = session.clock().now() + TIMEOUT;
                    match request {
                        Content::ExecCreate(_) => {
                            responder
                                .reply(schema::ExecutionCreateResponse { taskid: 7 }, deadline)
                                .unwrap();
                        }
                        Content::ExecLoad(_) => {
                            loaded.send(responder).unwrap();
                            if !expire {
                                session.close();
                                return false;
                            }
                        }
                        _ => panic!("unexpected request"),
                    }
                    true
                }),
            );
            let mut ark = attach(&mut peer);
            let worker = thread::spawn(move || {
                let mut progress = Vec::new();
                let result = run(&mut ark, &mut Cursor::new(b"abc"), |stage| {
                    progress.push(stage)
                });
                (result, progress)
            });

            // Leave the load unanswered until the deadline or disconnect
            let _loading = loads.recv().unwrap();
            if expire {
                tester.advance(TIMEOUT);
            }
            let (result, progress) = worker.join().unwrap();
            if expire {
                assert!(matches!(result, Err(Error::Timeout)));
            } else {
                assert!(matches!(result, Err(Error::Disconnected(_))));
            }
            assert!(
                !progress.contains(&ExecutionProgress::Authorizing),
                "{expire}"
            );
        }
    }

    /// Withheld reports keep the Ark's reserved verdict.
    #[test]
    fn test_withheld_reports() {
        let clock = test_clock().clock();
        for code in [
            schema::ReservedErrors::Unauthorized,
            schema::ReservedErrors::Unconfirmed,
            schema::ReservedErrors::Undelivered,
        ] {
            let expected = schema::Error::reserved(code, "report withheld");
            let (mut peer, observed) = peer(
                &clock,
                Some(("result", expected.clone())),
                vec![schema::ExecutionState::Resolved as i32],
                outcome(true),
                pull,
            );
            let mut ark = attach(&mut peer);
            assert!(
                matches!(run(&mut ark, &mut Cursor::new(b"abc"), |_| {}),
                Err(Error::Remote(error)) if error == expected),
                "{code:?}"
            );
            assert_eq!(
                observed.lock().unwrap().stages.last(),
                Some(&"cancel"),
                "{code:?}"
            );
        }
    }

    /// Empty, short and overrunning output chunks fail either stream.
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
                    pull,
                );
                let mut ark = attach(&mut peer);
                assert!(
                    matches!(
                        run(&mut ark, &mut Cursor::new(b"abc"), |_| {}),
                        Err(Error::Execution(_))
                    ),
                    "{stream:?}/{length}"
                );
                let observed = observed.lock().unwrap();
                assert_eq!(
                    observed.stages.last(),
                    Some(&"cancel"),
                    "{stream:?}/{length}"
                );
                assert_eq!(
                    observed.reads.last().unwrap().stream,
                    stream as i32,
                    "{stream:?}/{length}"
                );
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
                pull,
            );
            let mut ark = attach(&mut peer);
            assert_eq!(
                run(&mut ark, &mut Cursor::new(b"abc"), |_| {}).unwrap(),
                expected,
                "{stdout}/{stderr}"
            );
            assert_eq!(
                observed
                    .lock()
                    .unwrap()
                    .reads
                    .iter()
                    .map(|read| read.stream)
                    .collect::<Vec<_>>(),
                streams,
                "{stdout}/{stderr}"
            );
        }
    }

    /// Empty sources, past deadlines and timed-out reads create no task.
    #[test]
    fn test_source_failures() {
        /// Seekable source whose reads time out.
        struct TimedOut;
        impl Read for TimedOut {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::TimedOut.into())
            }
        }
        impl Seek for TimedOut {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }

        // The peer must never receive a task for any of these failures
        let clock = test_clock().clock();
        let mut peer = scripted(&clock, Box::new(|_, _, _| panic!("unexpected request")));
        let mut ark = attach(&mut peer);
        assert!(matches!(
            run(&mut ark, &mut Cursor::new(b""), |_| {}),
            Err(Error::Execution(_))
        ));
        assert!(matches!(
            ark.execute(&mut Cursor::new(b"abc"), clock.now(), |_| {}),
            Err(Error::Timeout)
        ));
        assert!(matches!(
            run(&mut ark, &mut TimedOut, |_| {}),
            Err(Error::Timeout)
        ));
    }
}
