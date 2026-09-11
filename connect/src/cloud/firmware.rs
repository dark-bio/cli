// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Published firmware and the authenticated update sequence for Arks.

use super::{Failure, PackageAuth, Services, http, relay};
use crate::{Error, schema};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use darkbio_wire::protocol::Requester;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::time::Instant;

const CHUNK_SIZE: usize = 1024 * 1024;

/// A published encrypted archive. The Ark verifies its signature, contents and
/// version before installation; the host checks the download's length and hash.
#[derive(Clone, Debug)]
pub struct Firmware {
    pub version: String,
    pub summary: String,
    pub published: String, // Publication time supplied by the package repository
    pub size: u64,
    pub sha256: [u8; 32],
}

impl Firmware {
    /// Whether this is a newer candidate for the installed version. Stable
    /// builds require a semantic version bump; develop builds may be replaced.
    /// The Ark decides whether it accepts an update.
    pub fn is_update_for(&self, installed: &str) -> Result<bool, Error> {
        let current = Version::parse(installed)?;
        let proposed = Version::parse(&self.version)?;
        Ok(proposed.numbers > current.numbers
            || (proposed.numbers == current.numbers && !current.stable))
    }

    fn path(&self) -> String {
        format!(
            "imgs/arkos-{}-{}.arch",
            self.version,
            hex::encode(self.sha256)
        )
    }
}

/// Update stages reported on the caller's thread. Uploaded bytes have been
/// acknowledged by the Ark; installation completes only when the call returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateProgress {
    /// Synchronization has completed; the Ark may now request approval.
    Preparing,
    /// Downloaded archive bytes acknowledged by the Ark so far.
    Uploading { uploaded: u64, total: u64 },
    /// The complete archive passed the host checks and is being verified by the Ark.
    Verifying,
    /// The verified firmware is being installed; success will reboot the Ark.
    Installing,
}

/// Ark versions carry three u16 components and a seven-character build suffix.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    numbers: [u16; 3],
    stable: bool, // A stable build follows develop at the same semantic version
    commit: String,
}

impl Version {
    fn parse(value: &str) -> Result<Self, Error> {
        let invalid = || Error::Firmware(format!("invalid firmware version {value:?}"));
        let (version, commit) = value.split_once('-').ok_or_else(invalid)?;
        let mut parts = version.split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            let part = parts.next().ok_or_else(invalid)?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            *number = part.parse().map_err(|_| invalid())?;
        }
        if parts.next().is_some()
            || commit.len() != 7
            || (commit != "develop" && !commit.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid());
        }
        Ok(Self {
            numbers,
            stable: commit != "develop",
            commit: commit.to_owned(),
        })
    }
}

#[derive(Deserialize)]
struct Listing {
    package: String,
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    version: String,
    summary: String,
    published: String,
    size: u64,
    sha256: String,
    path: String,
}

impl Listing {
    /// Validate routing before any archive or device access. Archive paths must
    /// name the same version and hash as the cloud access-key request.
    fn firmwares(self) -> Result<Vec<Firmware>, Error> {
        if self.package != "arkos" {
            return Err(Error::Firmware("package listing is not arkos".into()));
        }
        let mut firmwares = Vec::new();
        for artifact in self.artifacts {
            let version = Version::parse(&artifact.version)?;
            let mut sha256 = [0; 32];
            hex::decode_to_slice(&artifact.sha256, &mut sha256)
                .map_err(|_| Error::Firmware("invalid firmware SHA-256".into()))?;
            let firmware = Firmware {
                version: artifact.version,
                summary: artifact.summary,
                published: artifact.published,
                size: artifact.size,
                sha256,
            };
            if firmware.size == 0 || artifact.path.trim_start_matches('/') != firmware.path() {
                return Err(Error::Firmware(
                    "invalid firmware size or archive path".into(),
                ));
            }
            firmwares.push((version, firmware));
        }
        firmwares.sort_by(|(a, _), (b, _)| b.cmp(a));
        Ok(firmwares
            .into_iter()
            .map(|(_, firmware)| firmware)
            .collect())
    }
}

#[derive(Deserialize)]
struct Access {
    access: String, // Cloud response sealed to the Ark's ephemeral update key
}

impl Services {
    fn firmware_cloud(&self) -> Result<&http::Api, Error> {
        if let Some(error) = &self.state.lock().expect("cloud setup not poisoned").error {
            return Err(error.clone().into());
        }
        self.cloud.as_ref().ok_or(Error::Unattested)
    }

    pub(crate) fn firmwares(
        &self,
        deadline: Instant,
        auth: Option<&PackageAuth>,
    ) -> Result<Vec<Firmware>, Error> {
        let cloud = self.firmware_cloud()?;
        let listing: Listing = http::json(cloud.package("imgs/arkos.pkgs", auth, deadline)?)?;
        listing.firmwares()
    }

    /// Holds one update across its cloud requests and wire calls. A failure ends
    /// the sequence; upload chunks and installation are never retried implicitly.
    pub(crate) fn update_firmware(
        &self,
        requester: &Requester,
        firmware: &Firmware,
        deadline: Instant,
        mut progress: impl FnMut(UpdateProgress),
        auth: Option<&PackageAuth>,
    ) -> Result<(), Error> {
        let cloud = self.firmware_cloud()?;
        Version::parse(&firmware.version)?;
        if firmware.size == 0 {
            return Err(Error::Firmware("firmware archive is empty".into()));
        }
        let _updating = self
            .updating
            .try_lock()
            .map_err(|_| Error::Firmware("another firmware update is already running".into()))?;
        self.sync(requester, deadline)?;
        progress(UpdateProgress::Preparing);
        let prepared = requester
            .request(
                schema::FirmwareUpdatePrepRequest {
                    version: firmware.version.clone(),
                    sha256: firmware.sha256.to_vec(),
                    bytes: firmware.size,
                },
                deadline,
            )?
            .wait::<schema::FirmwareUpdatePrepResponse>()?;
        let access: Access = http::get(
            cloud
                .agent
                .get(format!("{}/firmware", cloud.url))
                .query("version", &firmware.version)
                .query("sha256", hex::encode(firmware.sha256))
                .header("Dark-Auth", BASE64_URL_SAFE_NO_PAD.encode(prepared.auth)),
            deadline,
        )?;
        let access = BASE64_STANDARD.decode(access.access).map_err(|error| {
            Error::Firmware(format!("invalid firmware access encoding: {error}"))
        })?;
        requester
            .request(schema::FirmwareUpdateInitRequest { access }, deadline)?
            .wait::<schema::FirmwareUpdateInitResponse>()?;

        progress(UpdateProgress::Uploading {
            uploaded: 0,
            total: firmware.size,
        });
        let mut response = cloud.package(&firmware.path(), auth, deadline)?;
        if !response.status().is_success() {
            return Err(Error::Cloud(format!(
                "firmware download returned HTTP {}",
                response.status()
            )));
        }
        upload(
            requester,
            &mut response.body_mut().as_reader(),
            firmware,
            deadline,
            &mut progress,
        )?;

        progress(UpdateProgress::Verifying);
        requester
            .request(schema::FirmwareUpdateVerifyRequest {}, deadline)?
            .wait::<schema::FirmwareUpdateVerifyResponse>()?;
        progress(UpdateProgress::Installing);
        requester
            .request(schema::FirmwareUpdateInstallRequest {}, deadline)?
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
    deadline: Instant,
    progress: &mut impl FnMut(UpdateProgress),
) -> Result<(), Error> {
    let mut uploaded = 0;
    let mut hash = Sha256::new();
    while uploaded < firmware.size {
        relay::remaining(deadline).map_err(relay::io_error)?;
        let size = (firmware.size - uploaded).min(CHUNK_SIZE as u64) as usize;
        let mut chunk = vec![0; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|error| Failure::from(ureq::Error::from(error)))?;
        hash.update(&chunk);
        requester
            .request(schema::FirmwareUpdateUploadRequest { chunk }, deadline)?
            .wait::<schema::FirmwareUpdateUploadResponse>()?;
        uploaded += size as u64;
        progress(UpdateProgress::Uploading {
            uploaded,
            total: firmware.size,
        });
    }
    relay::remaining(deadline).map_err(relay::io_error)?;
    if reader
        .read(&mut [0])
        .map_err(|error| Failure::from(ureq::Error::from(error)))?
        != 0
    {
        return Err(Error::Firmware(
            "archive exceeds its advertised size".into(),
        ));
    }
    if hash.finalize().as_slice() != firmware.sha256 {
        return Err(Error::Firmware(
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
    use serde_json::json;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    fn firmware(bytes: &[u8]) -> Firmware {
        Firmware {
            version: "2.0.0-1234567".into(),
            summary: "Test firmware".into(),
            published: "2026-09-11T00:00:00Z".into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    fn listing(firmware: &Firmware) -> serde_json::Value {
        json!({"package": "arkos", "artifacts": [{"version": firmware.version, "summary": firmware.summary, "published": firmware.published, "size": firmware.size, "sha256": hex::encode(firmware.sha256), "path": firmware.path()}]})
    }

    /// Stops accepting when the test finishes, including when a refused device
    /// request means later HTTP stages must never be contacted.
    struct Cloud {
        url: String,
        stop: mpsc::Sender<()>,
        worker: Option<thread::JoinHandle<Vec<String>>>,
    }

    impl Cloud {
        fn start(firmware: &Firmware, bytes: Vec<u8>, access_status: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let catalog = listing(firmware).to_string();
            let archive = format!("/{}", firmware.path());
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
                        "/imgs/arkos.pkgs" => (200, catalog.as_bytes()),
                        path if path == key => {
                            assert!(headers.to_lowercase().contains("dark-auth: -_8\r\n"));
                            (access_status, br#"{"access":"/w=="}"#.as_slice())
                        }
                        path if path == archive => (200, bytes.as_slice()),
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
        let cloud = Cloud::start(&expected, bytes, 200);
        let (mut peer, stages) = peer(&expected, None);
        let ark = attach(&mut peer, cloud.url.clone());
        let authentications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = authentications.clone();
        let client = ark.client().with_package_auth(move |_, redirect, _| {
            assert!(redirect.is_none());
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(("test-auth".into(), "private-token".into())))
        });
        let deadline = Instant::now() + TIMEOUT;
        let firmwares = client.firmwares(deadline).unwrap();
        assert_eq!(firmwares[0].sha256, expected.sha256);
        assert!(
            stages.lock().unwrap().is_empty(),
            "listing must not initialize the device"
        );
        let mut progress = Vec::new();
        client
            .clone()
            .update_firmware(&firmwares[0], deadline, |stage| {
                if stage == UpdateProgress::Preparing {
                    assert!(matches!(
                        client.update_firmware(&expected, deadline, |_| {}),
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
        assert_eq!(cloud.finish().len(), 5);
        assert_eq!(authentications.load(std::sync::atomic::Ordering::SeqCst), 2);
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
            let cloud = Cloud::start(&firmware, bytes, 200);
            let (mut peer, stages) = peer(&firmware, Some(failure));
            let ark = attach(&mut peer, cloud.url.clone());
            let error = ark
                .client()
                .update_firmware(&firmware, Instant::now() + TIMEOUT, |_| {})
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
            let cloud = Cloud::start(&firmware, bytes, 200);
            let (mut peer, stages) = peer(&firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            assert!(
                ark.client()
                    .update_firmware(&firmware, Instant::now() + TIMEOUT, |_| {})
                    .is_err()
            );
            assert!(!stages.lock().unwrap().contains(&"verify"));
            cloud.finish();
        }
    }

    /// A rejected access request cannot initialize an update. Expiring the shared
    /// deadline after transfer prevents verification from reaching the Ark.
    #[test]
    fn test_access_and_deadline() {
        for expire in [false, true] {
            let bytes = vec![42; 17];
            let firmware = firmware(&bytes);
            let cloud = Cloud::start(&firmware, bytes, if expire { 200 } else { 403 });
            let (mut peer, stages) = peer(&firmware, None);
            let ark = attach(&mut peer, cloud.url.clone());
            let deadline = Instant::now() + Duration::from_secs(1);
            let error = ark
                .client()
                .update_firmware(&firmware, deadline, |stage| {
                    if expire && stage == UpdateProgress::Verifying {
                        thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    }
                })
                .unwrap_err();
            if expire {
                assert!(matches!(error, Error::Timeout));
            } else {
                assert!(matches!(error, Error::Cloud(_)));
            }
            let stages = stages.lock().unwrap().clone();
            assert!(!stages.contains(if expire { &"verify" } else { &"init" }));
            cloud.finish();
        }
    }

    #[test]
    fn test_listing_and_versions() {
        let firmware = firmware(&[42]);
        for mutate in [
            |value: &mut serde_json::Value| value["package"] = json!("foreign"),
            |value: &mut serde_json::Value| {
                value["artifacts"][0]["version"] = json!("1.2.3.4-develop")
            },
            |value: &mut serde_json::Value| value["artifacts"][0]["sha256"] = json!("aa"),
            |value: &mut serde_json::Value| value["artifacts"][0]["size"] = json!(0),
            |value: &mut serde_json::Value| {
                value["artifacts"][0]["path"] = json!("https://foreign.invalid/firmware")
            },
        ] {
            let mut value = listing(&firmware);
            mutate(&mut value);
            assert!(
                serde_json::from_value::<Listing>(value)
                    .unwrap()
                    .firmwares()
                    .is_err()
            );
        }
        let mut value = listing(&firmware);
        for version in ["1.0.0-develop", "3.0.0-develop", "3.0.0-1234567"] {
            let mut next = firmware.clone();
            next.version = version.into();
            value["artifacts"]
                .as_array_mut()
                .unwrap()
                .push(listing(&next)["artifacts"][0].clone());
        }
        let sorted = serde_json::from_value::<Listing>(value)
            .unwrap()
            .firmwares()
            .unwrap();
        assert_eq!(
            sorted
                .iter()
                .map(|firmware| firmware.version.as_str())
                .collect::<Vec<_>>(),
            [
                "3.0.0-1234567",
                "3.0.0-develop",
                "2.0.0-1234567",
                "1.0.0-develop"
            ]
        );
        assert!(firmware.is_update_for("1.0.0-fffffff").unwrap());
        assert!(!firmware.is_update_for("2.0.0-0000000").unwrap());
        assert!(firmware.is_update_for("2.0.0-develop").unwrap());
        assert!(firmware.is_update_for("not-a-version").is_err());
    }

    /// An emulator receives the update request and supplies its own refusal.
    /// The refusal stops the sequence before access keys or archives are fetched.
    #[test]
    fn test_emulator_refusal() {
        let bytes = vec![42];
        let expected = firmware(&bytes);
        let cloud = Cloud::start(&expected, bytes, 200);
        let (mut peer, stages) = peer(&expected, Some("prepare"));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let mut services = Services::new(&identity);
        services.cloud = Some(http::tests::api(cloud.url.clone(), Realm::Emulator));
        let ark = crate::Ark::start(session, Arc::new(services)).unwrap();
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        let firmwares = client.firmwares(deadline).unwrap();
        let error = client
            .update_firmware(&firmwares[0], deadline, |_| {})
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
            [
                "/imgs/arkos.pkgs",
                "/v1/cloudsync/identity",
                "/v1/cloudsync/time?challenge=03"
            ]
        );
    }

    /// Firmware discovery needs an attested environment and a live owner.
    #[test]
    fn test_identity_and_closure() {
        let mut peer = Peer::spawn(Box::new(|_, _, _| panic!("unexpected device request")));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, identity) =
            darkbio_wire::protocol::connect(peer.stream(), &verifier).unwrap();
        let services = Services::new(&identity);
        let firmware = firmware(&[42]);
        let deadline = Instant::now() + TIMEOUT;
        assert!(matches!(
            services.firmwares(deadline, None),
            Err(Error::Unattested)
        ));
        assert!(matches!(
            services.update_firmware(&session.requester(), &firmware, deadline, |_| {}, None),
            Err(Error::Unattested)
        ));
        services.close();
        assert!(matches!(
            services.firmwares(deadline, None),
            Err(Error::Closed)
        ));
    }
}
