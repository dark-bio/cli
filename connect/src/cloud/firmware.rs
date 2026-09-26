// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Published firmware and the authenticated update sequence for Arks.

use super::{Failure, Services, http};
use crate::{Error, Timing, schema};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use darkbio_wire::protocol::Requester;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;

/// Archive bytes per acknowledged upload request, 1 MiB to amortize each round
/// trip.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Published encrypted firmware archive, as
/// [`Client::update_firmware`](crate::Client::update_firmware) streams it to the
/// Ark.
///
/// The host checks the archive's length and SHA-256 while streaming it, and
/// the Ark decrypts and verifies it before installation.
#[derive(Clone, Debug)]
pub struct Firmware {
    /// Published version authorized by the cloud and checked by the Ark.
    pub version: String,
    /// Exact encrypted archive length in bytes.
    pub size: u64,
    /// Expected SHA-256 of the encrypted archive, checked before verification.
    pub sha256: [u8; 32],
}

/// Stages of [`Client::update_firmware`](crate::Client::update_firmware),
/// reported on the caller's thread.
///
/// Uploaded bytes count only what the Ark acknowledged, and installation
/// completes only when the call returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateProgress {
    /// Preparation after cloud sync, during which the Ark may ask for approval.
    Preparing,
    /// Upload of the archive, with the bytes the Ark acknowledged so far.
    Uploading {
        /// Archive bytes acknowledged so far.
        uploaded: u64,
        /// Declared encrypted archive length in bytes.
        total: u64,
    },
    /// Verification by the Ark, once the complete archive passed the host's
    /// checks.
    Verifying,
    /// Installation of the verified firmware, which reboots the Ark on success.
    Installing,
}

/// Firmware authorization returned by the cloud for this prepared update.
#[derive(Deserialize)]
struct Access {
    /// Archive key sealed to the Ark's ephemeral update key, in standard base64.
    access: String,
}

impl Services {
    /// Returns the cloud route, failing first with the session's ending reason
    /// once it ended.
    fn firmware_cloud(&self) -> Result<&http::Api, Error> {
        if let Some(error) = &self.state.lock().expect("cloud setup not poisoned").error {
            return Err(error.clone().into());
        }
        self.cloud.as_ref().ok_or(Error::MissingEnvironment)
    }

    /// Runs one firmware update, from cloud authorization through installation.
    ///
    /// One update runs at a time across client clones. A failure ends the
    /// sequence, and upload chunks and installation are never retried.
    pub(crate) fn update_firmware(
        &self,
        requester: &Requester,
        firmware: &Firmware,
        reader: &mut impl Read,
        timing: Timing,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<(), Error> {
        // Admit one update of a non-empty archive at a time, on a synced Ark
        let cloud = self.firmware_cloud()?;
        if firmware.size == 0 {
            return Err(Error::Firmware("firmware archive is empty".into()));
        }
        let _updating = self
            .updating
            .try_lock()
            .map_err(|_| Error::Firmware("another firmware update is already running".into()))?;
        self.sync(requester, timing)?;
        let clock = &self.clock;

        // Finish any browser login before asking the Ark to prepare an update.
        // A protected host can need login even when device sync is still fresh.
        if cloud.auth.configured() {
            cloud.with_auth(timing, || cloud.identity(timing.io(clock)))?;
        }

        // Ask the Ark to prepare, which may wait for the owner's approval
        progress(UpdateProgress::Preparing);
        let prepared = requester
            .request(
                schema::FirmwareUpdatePrepRequest {
                    version: firmware.version.clone(),
                    sha256: firmware.sha256.to_vec(),
                    bytes: firmware.size,
                },
                timing.approval(clock),
            )?
            .wait::<schema::FirmwareUpdatePrepResponse>()?;

        // Fetch the archive key for the prepared update and hand it to the Ark
        let access: Access = match http::get_authenticated(
            cloud,
            cloud
                .agent
                .get(format!("{}/firmware", cloud.url))
                .query("version", &firmware.version)
                .query("sha256", hex::encode(firmware.sha256))
                .header("Dark-Auth", BASE64_URL_SAFE_NO_PAD.encode(prepared.auth)),
            timing.io(clock),
        ) {
            Err(Failure::AuthRequired) => {
                // Browser login can outlive the prepared proof. Leave a second
                // preparation and its possible approval to an explicit rerun.
                cloud.auth.login(&cloud.origin, clock, timing)?;
                return Err(Error::CloudAuth {
                    origin: cloud.origin.clone(),
                    message: "signed in to the cloud; rerun the firmware update".into(),
                });
            }
            Err(Failure::ProofRejected) => {
                // Preparation may already have required approval. Refresh keys for
                // the next attempt, leaving the caller to start it explicitly.
                self.resync(requester, timing)?;
                return Err(Error::ProofRejected);
            }
            result => result?,
        };
        let access = BASE64_STANDARD.decode(access.access).map_err(|error| {
            Error::Firmware(format!("invalid firmware access encoding: {error}"))
        })?;
        requester
            .request(
                schema::FirmwareUpdateInitRequest { access },
                timing.io(clock),
            )?
            .wait::<schema::FirmwareUpdateInitResponse>()?;

        // Stream the archive, checking its length and hash along the way
        progress(UpdateProgress::Uploading {
            uploaded: 0,
            total: firmware.size,
        });
        upload(requester, reader, firmware, timing, &mut progress)?;

        // Let the Ark verify the archive, then install it
        progress(UpdateProgress::Verifying);
        requester
            .request(schema::FirmwareUpdateVerifyRequest {}, timing.io(clock))?
            .wait::<schema::FirmwareUpdateVerifyResponse>()?;
        progress(UpdateProgress::Installing);
        requester
            .request(schema::FirmwareUpdateInstallRequest {}, timing.io(clock))?
            .wait::<schema::FirmwareUpdateInstallResponse>()?;
        Ok(())
    }
}

/// Streams one acknowledged chunk at a time, rejecting truncation, extra bytes
/// and a hash mismatch before asking the Ark to verify or install anything.
fn upload(
    requester: &Requester,
    reader: &mut impl Read,
    firmware: &Firmware,
    timing: Timing,
    progress: &mut impl FnMut(UpdateProgress),
) -> Result<(), Error> {
    // Send the declared bytes one acknowledged chunk at a time, hashing them
    let clock = &requester.clock();
    let mut uploaded = 0;
    let mut hash = Sha256::new();
    while uploaded < firmware.size {
        timing.check(clock)?;
        let size = (firmware.size - uploaded).min(CHUNK_SIZE as u64) as usize;
        let mut chunk = vec![0; size];
        reader.read_exact(&mut chunk).map_err(Error::FirmwareRead)?;
        hash.update(&chunk);
        requester
            .request(
                schema::FirmwareUpdateUploadRequest { chunk },
                timing.io(clock),
            )?
            .wait::<schema::FirmwareUpdateUploadResponse>()?;
        uploaded += size as u64;
        progress(UpdateProgress::Uploading {
            uploaded,
            total: firmware.size,
        });
    }

    // The source must end at the declared size and match the published hash
    timing.check(clock)?;
    if reader.read(&mut [0]).map_err(Error::FirmwareRead)? != 0 {
        return Err(Error::Integrity(
            "archive exceeds its advertised size".into(),
        ));
    }
    if hash.finalize().as_slice() != firmware.sha256 {
        return Err(Error::Integrity(
            "archive SHA-256 does not match the published firmware".into(),
        ));
    }
    Ok(())
}

/// Update sequencing, refusals, integrity checks and cloud access over real
/// wire peers.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::tests::{TIMEOUT, attach};
    use crate::schema::host_to_ark::Content;
    use crate::testing::{Peer, answering, test_clock};
    use crate::trust::Realm;
    use darkbio_clock::Clock;
    use std::io::Write;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Describes an archive holding `bytes` as published firmware.
    fn firmware(bytes: &[u8]) -> Firmware {
        Firmware {
            version: "2.0.0-1234567".into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    /// Loopback cloud serving sync and firmware access, recording the paths it
    /// serves.
    ///
    /// It stops accepting when the test finishes, including when a refused
    /// device request means later HTTP stages are never contacted.
    struct Cloud {
        /// Base URL of the loopback API.
        url: String,
        /// Listener address, which the stopping connection wakes.
        address: SocketAddr,
        /// Marks the next connection as the signal to stop.
        stopped: Arc<AtomicBool>,
        /// Serving thread, returning the paths it served.
        worker: Option<thread::JoinHandle<Vec<String>>>,
    }

    impl Cloud {
        /// Starts serving sync and the access key for `firmware`, answering the
        /// access request with `access_status`.
        fn start(firmware: &Firmware, _bytes: Vec<u8>, access_status: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let url = format!("http://{address}/v1");

            // The access route carries the version and hash in its query
            let key = format!(
                "/v1/firmware?version={}&sha256={}",
                firmware.version,
                hex::encode(firmware.sha256)
            );
            let stopped = Arc::new(AtomicBool::new(false));
            let worker = thread::spawn({
                let stopped = stopped.clone();
                move || {
                    let mut paths = Vec::new();
                    loop {
                        // A connection after the stop flag is the signal to end
                        let (mut stream, _) = listener.accept().unwrap();
                        if stopped.load(Ordering::SeqCst) {
                            break paths;
                        }

                        // Answer the routes of one update and record each path
                        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                        let headers = headers(&mut stream);
                        let path = headers.split_whitespace().nth(1).unwrap();
                        let (status, body) = match path {
                            "/v1/cloudsync/identity" => {
                                (200, br#"{"signer":"AQ==","crypto":"Ag=="}"#.as_slice())
                            }
                            "/v1/cloudsync/time?challenge=03" => {
                                (200, br#"{"unixmilli":123,"signature":"BA=="}"#.as_slice())
                            }
                            path if path == key => {
                                assert!(headers.to_lowercase().contains("dark-auth: -_8\r\n"));
                                (access_status, br#"{"access":"/w=="}"#.as_slice())
                            }
                            other => panic!("unexpected HTTP request: {other}"),
                        };
                        paths.push(path.to_owned());
                        let header = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        // The client may hang up before the response is written
                        let _ = stream
                            .write_all(header.as_bytes())
                            .and_then(|()| stream.write_all(body));
                    }
                }
            });
            Self {
                url,
                address,
                stopped,
                worker: Some(worker),
            }
        }

        /// Stops the server and returns the paths it served, in order.
        fn finish(mut self) -> Vec<String> {
            self.stop();
            self.worker.take().unwrap().join().unwrap()
        }

        /// Wakes the accept loop with a connection of its own, which it takes as
        /// the signal to stop.
        fn stop(&self) {
            self.stopped.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
        }
    }

    impl Drop for Cloud {
        /// Stops a server the test did not finish, joining its thread.
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                self.stop();
                let _ = worker.join();
            }
        }
    }

    /// Reads a request's headers one byte at a time, up to the blank line.
    fn headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    }

    /// Spawns an Ark peer that records the update stages it serves, refusing
    /// the stage `fail` names.
    ///
    /// A `fail` of `"disconnect"` hangs up at installation instead. Verification
    /// checks that the whole archive arrived.
    fn peer(
        clock: &Clock,
        firmware: &Firmware,
        fail: Option<&'static str>,
    ) -> (Peer, Arc<Mutex<Vec<&'static str>>>) {
        let firmware = firmware.clone();
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        let mut uploaded = Vec::new();
        let peer = Peer::spawn(
            clock,
            Box::new(move |session, request, responder| {
                let (stage, response): (_, darkbio_wire::protocol::Message) = match request {
                    Content::CloudSyncStart(request) => {
                        assert_eq!((request.signer, request.crypto), (vec![1], vec![2]));
                        (
                            "sync-start",
                            schema::CloudSyncStartResponse { challenge: vec![3] }.into(),
                        )
                    }
                    Content::CloudSyncFinish(request) => {
                        assert_eq!((request.unixmilli, request.signature), (123, vec![4]));
                        (
                            "sync-finish",
                            schema::CloudSyncFinishResponse { accepted: 123 }.into(),
                        )
                    }
                    Content::FirmwareUpdatePrep(request) => {
                        assert_eq!(request.version, firmware.version);
                        assert_eq!(request.sha256, firmware.sha256);
                        assert_eq!(request.bytes, firmware.size);
                        (
                            "prepare",
                            schema::FirmwareUpdatePrepResponse {
                                auth: vec![0xfb, 0xff],
                            }
                            .into(),
                        )
                    }
                    Content::FirmwareUpdateInit(request) => {
                        assert_eq!(request.access, [0xff]);
                        ("init", schema::FirmwareUpdateInitResponse {}.into())
                    }
                    Content::FirmwareUpdateUpload(request) => {
                        assert!(!request.chunk.is_empty() && request.chunk.len() <= CHUNK_SIZE);
                        uploaded.extend(request.chunk);
                        ("upload", schema::FirmwareUpdateUploadResponse {}.into())
                    }
                    Content::FirmwareUpdateVerify(_) => {
                        assert_eq!(uploaded.len() as u64, firmware.size);
                        assert_eq!(Sha256::digest(&uploaded).as_slice(), firmware.sha256);
                        ("verify", schema::FirmwareUpdateVerifyResponse {}.into())
                    }
                    Content::FirmwareUpdateInstall(_) => {
                        ("install", schema::FirmwareUpdateInstallResponse {}.into())
                    }
                    other => return answering(session, other, responder),
                };

                // Record the stage, then hang up, refuse or answer as asked
                observed.lock().unwrap().push(stage);
                if fail == Some("disconnect") && stage == "install" {
                    return false;
                }
                let deadline = session.clock().now() + TIMEOUT;
                if fail == Some(stage) {
                    responder
                        .fail(schema::Error::new(0x777, "test update refusal"), deadline)
                        .unwrap();
                } else {
                    responder.reply(response, deadline).unwrap();
                }
                true
            }),
        );
        (peer, stages)
    }

    /// An update runs every stage in order, reports progress per chunk and
    /// refuses a concurrent update.
    #[test]
    fn test_update() {
        // Serve an archive of two chunks through a cloud and Ark accepting all
        let clock = test_clock().clock();
        let bytes = vec![42; CHUNK_SIZE + 17];
        let expected = firmware(&bytes);
        let cloud = Cloud::start(&expected, bytes.clone(), 200);
        let (mut peer, stages) = peer(&clock, &expected, None);
        let ark = attach(&mut peer, cloud.url.clone());
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;

        // Update, starting a second update while the first one prepares
        let mut progress = Vec::new();
        client
            .clone()
            .update_firmware(&expected, &mut bytes.as_slice(), deadline, |stage| {
                if stage == UpdateProgress::Preparing {
                    assert!(matches!(
                        client.update_firmware(&expected, &mut bytes.as_slice(), deadline, |_| {}),
                        Err(Error::Firmware(_))
                    ));
                }
                progress.push(stage);
            })
            .unwrap();

        // The Ark saw every stage in order, progress tracked each chunk, and the
        // cloud served sync and one access request
        assert_eq!(
            *stages.lock().unwrap(),
            [
                "sync-start",
                "sync-finish",
                "prepare",
                "init",
                "upload",
                "upload",
                "verify",
                "install"
            ]
        );
        assert_eq!(
            progress,
            [
                UpdateProgress::Preparing,
                UpdateProgress::Uploading {
                    uploaded: 0,
                    total: expected.size
                },
                UpdateProgress::Uploading {
                    uploaded: CHUNK_SIZE as u64,
                    total: expected.size
                },
                UpdateProgress::Uploading {
                    uploaded: expected.size,
                    total: expected.size
                },
                UpdateProgress::Verifying,
                UpdateProgress::Installing
            ]
        );
        assert_eq!(cloud.finish().len(), 3);
    }

    /// Refusal at any stage prevents later stages, and losing the install reply
    /// does not count as a successful reboot or trigger an installation retry.
    #[test]
    fn test_refusals() {
        let clock = test_clock().clock();
        for failure in [
            "prepare",
            "init",
            "upload",
            "verify",
            "install",
            "disconnect",
        ] {
            let bytes = vec![42; 17];
            let firmware = firmware(&bytes);
            let cloud = Cloud::start(&firmware, bytes.clone(), 200);
            let (mut peer, stages) = peer(&clock, &firmware, Some(failure));
            let ark = attach(&mut peer, cloud.url.clone());
            let error = ark
                .client()
                .update_firmware(
                    &firmware,
                    &mut bytes.as_slice(),
                    clock.now() + TIMEOUT,
                    |_| {},
                )
                .unwrap_err();
            if failure != "disconnect" {
                assert!(matches!(error, Error::Remote(error) if error.code == 0x777));
            }

            // The failing stage came last, and installation ran at most once
            let stages = stages.lock().unwrap().clone();
            assert_eq!(
                stages.last().copied(),
                Some(if failure == "disconnect" {
                    "install"
                } else {
                    failure
                })
            );
            assert_eq!(
                stages.iter().filter(|&&stage| stage == "install").count(),
                usize::from(matches!(failure, "install" | "disconnect"))
            );
            cloud.finish();
        }
    }

    /// Truncated, oversized or corrupt archives never reach verification or
    /// installation, even after the Ark accepted earlier upload chunks.
    #[test]
    fn test_download_integrity() {
        let clock = test_clock().clock();
        for bytes in [vec![42; 16], vec![42; 18], vec![43; 17]] {
            let firmware = firmware(&[42; 17]);
            let cloud = Cloud::start(&firmware, bytes.clone(), 200);
            let (mut peer, stages) = peer(&clock, &firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            assert!(
                ark.client()
                    .update_firmware(
                        &firmware,
                        &mut bytes.as_slice(),
                        clock.now() + TIMEOUT,
                        |_| {}
                    )
                    .is_err()
            );
            assert!(!stages.lock().unwrap().contains(&"verify"));
            cloud.finish();
        }
    }

    /// A refused proof refreshes keys without preparing again or uploading, and
    /// a deadline passing after the transfer prevents verification.
    #[test]
    fn test_access_and_deadline() {
        let mut tester = test_clock();
        let clock = tester.clock();
        for expire in [false, true] {
            let bytes = vec![42; 17];
            let firmware = firmware(&bytes);
            let cloud = Cloud::start(&firmware, bytes.clone(), if expire { 200 } else { 403 });
            let (mut peer, stages) = peer(&clock, &firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            let deadline = clock.now() + Duration::from_secs(1);

            // The cloud refuses the proof, or the deadline passes at verification
            let error = ark
                .client()
                .update_firmware(&firmware, &mut bytes.as_slice(), deadline, |stage| {
                    if expire && stage == UpdateProgress::Verifying {
                        tester.advance_to(deadline);
                    }
                })
                .unwrap_err();
            if expire {
                assert!(matches!(error, Error::Timeout));
            } else {
                assert!(matches!(error, Error::ProofRejected));
            }
            let stages = stages.lock().unwrap().clone();
            assert!(!stages.contains(if expire { &"verify" } else { &"init" }));
            let paths = cloud.finish();

            // A refused proof resyncs once and never prepares again
            if !expire {
                assert_eq!(
                    stages,
                    [
                        "sync-start",
                        "sync-finish",
                        "prepare",
                        "sync-start",
                        "sync-finish"
                    ]
                );
                assert_eq!(paths.len(), 5);
                assert_eq!(
                    &paths[3..],
                    ["/v1/cloudsync/identity", "/v1/cloudsync/time?challenge=03"]
                );
            }
        }
    }

    /// A needed login runs before preparation, and one needed after it ends the
    /// update for a rerun instead of preparing again.
    #[test]
    fn caller_login_precedes_preparation_and_never_repeats_approval() {
        use crate::cloud::{
            auth::tests::{Login, refused},
            tests::{response, serve, sync_responses},
        };
        let clock = test_clock().clock();
        for expires_after_preparation in [false, true] {
            // The cloud refuses the caller's credentials either at the check
            // before preparation or at the access request after it
            let bytes = vec![42; 17];
            let firmware = firmware(&bytes);
            let mut responses = sync_responses();
            if !expires_after_preparation {
                responses.push(refused(302));
            }
            responses.push(sync_responses().remove(0));
            responses.push(if expires_after_preparation {
                refused(403)
            } else {
                response(200, r#"{"access":"/w=="}"#)
            });
            let (url, requests) = serve(responses);

            // Update through the login stand-in, noting its logins at preparation
            let (mut peer, stages) = peer(&clock, &firmware, None);
            let mut ark = attach(&mut peer, url);
            let login = Login::default();
            ark.set_cloud_auth(login.clone());
            let result = ark.client().update_firmware(
                &firmware,
                &mut bytes.as_slice(),
                Timing::inactivity(TIMEOUT),
                |stage| {
                    if stage == UpdateProgress::Preparing {
                        assert_eq!(
                            login.logins.load(Ordering::SeqCst),
                            usize::from(!expires_after_preparation)
                        );
                    }
                },
            );

            // The Ark prepared and synced once, and the stand-in logged in once
            let stages = stages.lock().unwrap();
            assert_eq!(
                stages.iter().filter(|stage| **stage == "prepare").count(),
                1
            );
            assert_eq!(
                stages
                    .iter()
                    .filter(|stage| **stage == "sync-start")
                    .count(),
                1
            );
            assert_eq!(login.logins.load(Ordering::SeqCst), 1);

            // A login after preparation ends the update for a rerun, while one
            // before it lets the update finish
            if expires_after_preparation {
                assert!(
                    matches!(result, Err(Error::CloudAuth { message, .. }) if message.contains("rerun"))
                );
                assert!(!stages.contains(&"init"));
                assert_eq!(requests.try_iter().count(), 4);
            } else {
                result.unwrap();
                assert!(stages.contains(&"install"));
                assert_eq!(requests.try_iter().count(), 5);
            }
        }
    }

    /// An emulator's own refusal of the update stops the sequence before the
    /// access key is requested.
    #[test]
    fn test_emulator_refusal() {
        // Route an unattested peer that refuses preparation to the emulator realm
        let clock = test_clock().clock();
        let bytes = vec![42];
        let expected = firmware(&bytes);
        let cloud = Cloud::start(&expected, bytes.clone(), 200);
        let (mut peer, stages) = peer(&clock, &expected, Some("prepare"));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let mut services = Services::new(&identity, None, &session.clock());
        services.cloud = Some(http::tests::api(
            cloud.url.clone(),
            Realm::Emulator,
            &session.clock(),
        ));
        let ark = crate::Ark::start(session, Arc::new(services)).unwrap();
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;

        // The refusal comes back unchanged, and the cloud served only sync
        let error = client
            .update_firmware(&expected, &mut bytes.as_slice(), deadline, |_| {})
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Remote(error) if error == schema::Error::new(0x777, "test update refusal")
        ));
        assert_eq!(
            *stages.lock().unwrap(),
            ["sync-start", "sync-finish", "prepare"]
        );
        assert_eq!(
            cloud.finish(),
            ["/v1/cloudsync/identity", "/v1/cloudsync/time?challenge=03"]
        );
    }

    /// An update fails without a cloud route, and with the session's ending
    /// reason once it closed.
    #[test]
    fn test_identity_and_closure() {
        // Without a cloud route, every update fails the same way
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(
            &clock,
            Box::new(|_, _, _| panic!("unexpected device request")),
        );
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let services = Services::new(&identity, None, &session.clock());
        let firmware = firmware(&[42]);
        let deadline = clock.now() + TIMEOUT;
        assert!(matches!(
            services.update_firmware(
                &session.requester(),
                &firmware,
                &mut [42].as_slice(),
                deadline.into(),
                |_| {}
            ),
            Err(Error::MissingEnvironment)
        ));
        assert!(matches!(
            services.update_firmware(
                &session.requester(),
                &firmware,
                &mut [42].as_slice(),
                deadline.into(),
                |_| {}
            ),
            Err(Error::MissingEnvironment)
        ));

        // Once closed, the ending reason comes first
        services.close();
        assert!(matches!(
            services.update_firmware(
                &session.requester(),
                &firmware,
                &mut [42].as_slice(),
                deadline.into(),
                |_| {}
            ),
            Err(Error::Closed)
        ));
    }
}
