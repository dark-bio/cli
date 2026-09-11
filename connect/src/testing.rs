// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Scripted wire peers and attestations for connection and trust tests.

use crate::{Ark, Error, Identity, TrustMode};
use darkbio_crypto::cwt::claims::{self, eat};
use darkbio_crypto::{cwt, xdsa};
use darkbio_trust::CRYPTO_DOMAIN_DEVICE_ATTESTATION;
use darkbio_trust::device::HardwareClaims;
use darkbio_wire::memory::{self, Duplex};
use darkbio_wire::protocol::schema::host_to_ark::Content as Request;
use darkbio_wire::protocol::schema::{self, DeviceInfoResponse, UnlockResponse};
use darkbio_wire::protocol::{Responder, Server, Session};
use darkbio_wire::transport::Attestation;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Bytes buffered per direction of a peer's stream, enough for the handshake
/// and a few messages to flow without the other side reading.
const CAPACITY: usize = 256 * 1024;

/// Hardware attestation of an identity, signed by the given key. Signed by the
/// identity itself, it is the placeholder of an Ark that was never onboarded.
pub fn self_attestation(signer: &xdsa::SecretKey, identity: xdsa::PublicKey) -> Attestation {
    let claims = HardwareClaims {
        sub: claims::Subject {
            sub: "test-device".into(),
        },
        cnf: claims::Confirm::new(identity),
        nbf: claims::NotBefore { nbf: 0 },
        iat: claims::IssuedAt { iat: 0 },
        oem: eat::Oemid::new_pen(0),
        hwm: eat::HwModel { hw_model: vec![] },
        hwv: eat::HwVersion::new("test-version".into()),
    };
    let cwt = cwt::issue(&claims, signer, CRYPTO_DOMAIN_DEVICE_ATTESTATION).unwrap();
    Attestation::new(cwt).unwrap()
}

/// Handles one peer request, with access to the session for reverse requests.
/// Returning false ends the peer's session.
pub type Script = Box<dyn FnMut(&Session, Request, Responder) -> bool + Send>;

/// Script answering device info requests with a firmware version, unlock
/// requests with a request of the peer's own ahead of the reply, and anything
/// else with the protocol's UNSUPPORTED error.
pub fn answering(session: &Session, request: Request, responder: Responder) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    let queued = match request {
        Request::DeviceInfo(_) => responder.reply(
            DeviceInfoResponse {
                firmware_version: "1.0.0".into(),
                ..Default::default()
            },
            deadline,
        ),
        Request::Unlock(_) => {
            let _ = session
                .requester()
                .request(DeviceInfoResponse::default(), deadline);
            responder.reply(UnlockResponse::default(), deadline)
        }
        _ => responder.fail(
            schema::Error::reserved(
                schema::ReservedErrors::Unsupported,
                "request not supported by test peer",
            ),
            deadline,
        ),
    };
    queued.is_ok()
}

/// Script never answering, holding on to the responders so the wire does not
/// answer for them either.
pub fn silent() -> Script {
    let mut held = Vec::new();
    Box::new(move |_, _, responder| {
        held.push(responder);
        true
    })
}

/// Script hanging up on the first request, holding on to its responder so
/// nothing but the end of the session reaches the client.
pub fn hangup() -> Script {
    let mut held = Vec::new();
    Box::new(move |_, _, responder| {
        held.push(responder);
        false
    })
}

/// Scripted Ark peer over an in-memory stream. Runs until its script finishes
/// or the host closes, and joins its serving thread on drop.
pub struct Peer {
    pub identity: xdsa::PublicKey, // Identity key the peer signs its handshake with
    stream: Option<Duplex>,        // Client's end of the stream, until taken
    thread: Option<JoinHandle<()>>, // Serving thread, joined on drop
}

impl Peer {
    /// Starts a peer serving the client per the script.
    pub fn spawn(mut script: Script) -> Self {
        let signer = xdsa::SecretKey::generate();
        let identity = signer.public_key();
        let attestation = self_attestation(&signer, identity.clone());

        let (host, ark) = memory::duplex(CAPACITY);
        let thread = thread::spawn(move || {
            let mut server = Server::new(ark, signer, attestation);
            let Ok(mut session) = server.accept() else {
                return;
            };
            while let Ok((message, responder)) = session.recv() {
                let Ok(request) = Request::try_from(message) else {
                    continue;
                };
                if !script(&session, request, responder) {
                    return;
                }
            }
        });
        Self {
            identity,
            stream: Some(host),
            thread: Some(thread),
        }
    }

    /// Takes the host stream for attachment or forwarding through another transport.
    pub fn stream(&mut self) -> Duplex {
        self.stream.take().expect("stream already taken")
    }

    /// Attaches to the peer using its pinned identity key.
    pub fn attach(&mut self) -> Result<(Ark, Identity), Error> {
        let stream = self.stream();
        Ark::attach(stream, &TrustMode::Recover(Box::new(self.identity.clone())))
    }
}

impl Drop for Peer {
    /// Releases an unused host stream and joins the peer after its session ends.
    fn drop(&mut self) {
        // An untaken host stream must close before joining the peer's receive loop.
        drop(self.stream.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
