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
#[cfg(test)]
use std::time::Instant;

/// Archive bytes per acknowledged transfer, amortizing USB and device write latency.
const CHUNK_SIZE: usize = 1024 * 1024;

/// A published encrypted archive. The Ark verifies its signature, contents and
/// version before installation; the host checks the download's length and hash.
#[derive(Clone, Debug)]
pub struct Firmware {
    /// Published version authorized by the cloud and checked by the Ark.
    pub version: String,
    /// Exact encrypted archive length in bytes.
    pub size: u64,
    /// Expected SHA-256 of the encrypted archive, checked before verification.
    pub sha256: [u8; 32],
}

/// Update stages reported on the caller's thread. Uploaded bytes have been
/// acknowledged by the Ark; installation completes only when the call returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateProgress {
    /// Synchronization has completed; the Ark may now request approval.
    Preparing,
    /// Downloaded archive bytes acknowledged by the Ark so far.
    Uploading {
        /// Archive bytes acknowledged so far.
        uploaded: u64,
        /// Declared encrypted archive length in bytes.
        total: u64,
    },
    /// The complete archive passed the host checks and is being verified by the Ark.
    Verifying,
    /// The verified firmware is being installed; success will reboot the Ark.
    Installing,
}

/// Firmware authorization returned by the cloud for this prepared update.
#[derive(Deserialize)]
struct Access {
    access: String, // Cloud response sealed to the Ark's ephemeral update key
}

impl Services {
    /// Requires a cloud route and preserves the owning session's ending reason.
    fn firmware_cloud(&self) -> Result<&http::Api, Error> {
        if let Some(error) = &self.state.lock().expect("cloud setup not poisoned").error {
            return Err(error.clone().into());
        }
        self.cloud.as_ref().ok_or(Error::MissingEnvironment)
    }

    /// Holds one update across its cloud requests and wire calls. A failure ends
    /// the sequence; upload chunks and installation are never retried implicitly.
    pub(crate) fn update_firmware(
        &self,
        requester: &Requester,
        firmware: &Firmware,
        reader: &mut impl Read,
        timing: Timing,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<(), Error> {
        let cloud = self.firmware_cloud()?;
        if firmware.size == 0 {
            return Err(Error::Firmware("firmware archive is empty".into()));
        }
        let _updating = self
            .updating
            .try_lock()
            .map_err(|_| Error::Firmware("another firmware update is already running".into()))?;
        self.sync(requester, timing)?;
        progress(UpdateProgress::Preparing);
        let prepared = requester
            .request(
                schema::FirmwareUpdatePrepRequest {
                    version: firmware.version.clone(),
                    sha256: firmware.sha256.to_vec(),
                    bytes: firmware.size,
                },
                timing.approval(),
            )?
            .wait::<schema::FirmwareUpdatePrepResponse>()?;
        let access: Access = match http::get_authenticated(
            cloud
                .agent
                .get(format!("{}/firmware", cloud.url))
                .query("version", &firmware.version)
                .query("sha256", hex::encode(firmware.sha256))
                .header("Dark-Auth", BASE64_URL_SAFE_NO_PAD.encode(prepared.auth)),
            timing.io(),
        ) {
            Err(Failure::ProofRejected) => {
                // Preparation may already have required approval. Refresh keys
                // for the next attempt, leaving the caller to start it explicitly.
                self.resync(requester, timing)?;
                return Err(Error::ProofRejected);
            }
            result => result?,
        };
        let access = BASE64_STANDARD.decode(access.access).map_err(|error| {
            Error::Firmware(format!("invalid firmware access encoding: {error}"))
        })?;
        requester
            .request(schema::FirmwareUpdateInitRequest { access }, timing.io())?
            .wait::<schema::FirmwareUpdateInitResponse>()?;

        progress(UpdateProgress::Uploading {
            uploaded: 0,
            total: firmware.size,
        });
        upload(requester, reader, firmware, timing, &mut progress)?;

        progress(UpdateProgress::Verifying);
        requester
            .request(schema::FirmwareUpdateVerifyRequest {}, timing.io())?
            .wait::<schema::FirmwareUpdateVerifyResponse>()?;
        progress(UpdateProgress::Installing);
        requester
            .request(schema::FirmwareUpdateInstallRequest {}, timing.io())?
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
    let mut uploaded = 0;
    let mut hash = Sha256::new();
    while uploaded < firmware.size {
        timing.check()?;
        let size = (firmware.size - uploaded).min(CHUNK_SIZE as u64) as usize;
        let mut chunk = vec![0; size];
        reader.read_exact(&mut chunk).map_err(Error::FirmwareRead)?;
        hash.update(&chunk);
        requester
            .request(schema::FirmwareUpdateUploadRequest { chunk }, timing.io())?
            .wait::<schema::FirmwareUpdateUploadResponse>()?;
        uploaded += size as u64;
        progress(UpdateProgress::Uploading {
            uploaded,
            total: firmware.size,
        });
    }
    timing.check()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::tests::{TIMEOUT, attach};
    use crate::schema::host_to_ark::Content;
    use crate::testing::{Peer, answering};
    use crate::trust::Realm;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    fn firmware(bytes: &[u8]) -> Firmware {
        Firmware {
            version: "2.0.0-1234567".into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    /// Stops accepting when the test finishes, including when a refused device
    /// request means later HTTP stages must never be contacted.
    struct Cloud {
        url: String,
        stop: mpsc::Sender<()>,
        worker: Option<thread::JoinHandle<Vec<String>>>,
    }

    impl Cloud {
        fn start(firmware: &Firmware, _bytes: Vec<u8>, access_status: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let key = format!(
                "/v1/firmware?version={}&sha256={}",
                firmware.version,
                hex::encode(firmware.sha256)
            );
            let (stop, stopped) = mpsc::channel();
            let worker = thread::spawn(move || {
                let mut paths = Vec::new();
                let deadline = Instant::now() + TIMEOUT;
                while stopped.try_recv().is_err() && Instant::now() < deadline {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("test cloud: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
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
                    // A failed wire upload may close the download mid-response.
                    let _ = stream
                        .write_all(header.as_bytes())
                        .and_then(|()| stream.write_all(body));
                }
                paths
            });
            Self {
                url,
                stop,
                worker: Some(worker),
            }
        }

        fn finish(mut self) -> Vec<String> {
            let _ = self.stop.send(());
            self.worker.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Cloud {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    }

    /// Records the update sequence and optionally refuses or disconnects during
    /// one stage. Only a fully uploaded archive is eligible for verification.
    fn peer(
        firmware: &Firmware,
        fail: Option<&'static str>,
    ) -> (Peer, Arc<Mutex<Vec<&'static str>>>) {
        let firmware = firmware.clone();
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        let mut uploaded = Vec::new();
        let peer = Peer::spawn(Box::new(move |session, request, responder| {
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
            observed.lock().unwrap().push(stage);
            if fail == Some("disconnect") && stage == "install" {
                return false;
            }
            let deadline = Instant::now() + TIMEOUT;
            if fail == Some(stage) {
                responder
                    .fail(schema::Error::new(0x777, "test update refusal"), deadline)
                    .unwrap();
            } else {
                responder.reply(response, deadline).unwrap();
            }
            true
        }));
        (peer, stages)
    }

    #[test]
    fn test_update() {
        let bytes = vec![42; CHUNK_SIZE + 17];
        let expected = firmware(&bytes);
        let cloud = Cloud::start(&expected, bytes.clone(), 200);
        let (mut peer, stages) = peer(&expected, None);
        let ark = attach(&mut peer, cloud.url.clone());
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
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
            let (mut peer, stages) = peer(&firmware, Some(failure));
            let ark = attach(&mut peer, cloud.url.clone());
            let error = ark
                .client()
                .update_firmware(
                    &firmware,
                    &mut bytes.as_slice(),
                    Instant::now() + TIMEOUT,
                    |_| {},
                )
                .unwrap_err();
            if failure != "disconnect" {
                assert!(matches!(error, Error::Remote(error) if error.code == 0x777));
            }
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

    /// Truncated, oversized or corrupt downloads never reach verification or
    /// installation, even if earlier upload chunks were accepted by the device.
    #[test]
    fn test_download_integrity() {
        for bytes in [vec![42; 16], vec![42; 18], vec![43; 17]] {
            let firmware = firmware(&[42; 17]);
            let cloud = Cloud::start(&firmware, bytes.clone(), 200);
            let (mut peer, stages) = peer(&firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            assert!(
                ark.client()
                    .update_firmware(
                        &firmware,
                        &mut bytes.as_slice(),
                        Instant::now() + TIMEOUT,
                        |_| {}
                    )
                    .is_err()
            );
            assert!(!stages.lock().unwrap().contains(&"verify"));
            cloud.finish();
        }
    }

    /// A rejected proof refreshes keys without repeating preparation or starting
    /// an upload. Expiring the deadline after transfer also prevents verification.
    #[test]
    fn test_access_and_deadline() {
        for expire in [false, true] {
            let bytes = vec![42; 17];
            let firmware = firmware(&bytes);
            let cloud = Cloud::start(&firmware, bytes.clone(), if expire { 200 } else { 403 });
            let (mut peer, stages) = peer(&firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            let deadline = Instant::now() + Duration::from_secs(1);
            let error = ark
                .client()
                .update_firmware(&firmware, &mut bytes.as_slice(), deadline, |stage| {
                    if expire && stage == UpdateProgress::Verifying {
                        thread::sleep(deadline.saturating_duration_since(Instant::now()));
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

    /// An emulator receives the update request and supplies its own refusal.
    /// The refusal stops the sequence before access keys or archives are fetched.
    #[test]
    fn test_emulator_refusal() {
        let bytes = vec![42];
        let expected = firmware(&bytes);
        let cloud = Cloud::start(&expected, bytes.clone(), 200);
        let (mut peer, stages) = peer(&expected, Some("prepare"));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let mut services = Services::new(&identity, None);
        services.cloud = Some(http::tests::api(cloud.url.clone(), Realm::Emulator));
        let ark = crate::Ark::start(session, Arc::new(services)).unwrap();
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
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

    /// Firmware discovery needs a selected environment and a live owner.
    #[test]
    fn test_identity_and_closure() {
        let mut peer = Peer::spawn(Box::new(|_, _, _| panic!("unexpected device request")));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let services = Services::new(&identity, None);
        let firmware = firmware(&[42]);
        let deadline = Instant::now() + TIMEOUT;
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
