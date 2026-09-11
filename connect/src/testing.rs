// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Peers for the tests, the wire's server over an in-memory stream answering
//! the client per a script, with the attestations the trust tests need.

use crate::Error;
use crate::ark::{Ark, DEFAULT_TIMEOUT, Realm};
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
use std::time::Instant;

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

/// Script deciding how a peer answers each request it receives, the session
/// there to send requests of its own through, returning whether to keep
/// serving.
pub type Script = Box<dyn FnMut(&Session, Request, Responder) -> bool + Send>;

/// Script answering device info requests with a firmware version, unlock
/// requests with a request of the peer's own ahead of the reply, and anything
/// else with an error.
pub fn answering(session: &Session, request: Request, responder: Responder) -> bool {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
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
        _ => responder.fail(schema::Error::new(7, "nope"), deadline),
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

/// The Ark's side of a session in the tests, the wire's server over one end
/// of an in-memory stream, serving the client on the other end per its script
/// until the script hangs up or the client goes away.
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

    /// Takes the client's end of the stream, to attach through or to carry
    /// over a transport of the test's own.
    pub fn stream(&mut self) -> Duplex {
        self.stream.take().expect("stream already taken")
    }

    /// Attaches a client to the peer over the stream, trusting its pinned
    /// identity.
    pub fn attach(&mut self) -> Result<(Ark, Attestation), Error> {
        let stream = self.stream();
        Ark::attach(stream, Realm::Sandbox, &self.identity)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // The serving thread ends with the session, which the client's end
        // of the stream going away ends if nobody attached
        drop(self.stream.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
