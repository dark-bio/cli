// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Scripted wire peers, attestations and test clocks for connection and trust tests.

use crate::{Ark, Error, Identity, TrustMode};
use darkbio_clock::{Clock, TestClock};
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
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Bytes buffered per direction of a peer's stream, enough for the handshake
/// and a few messages to flow without the other side reading.
const CAPACITY: usize = 256 * 1024;

/// Creates a stopped clock a day ahead of real time, so a stray read of the
/// real clock stands out from the test's time.
pub fn test_clock() -> TestClock {
    let mut tester = TestClock::new();
    tester.advance(Duration::from_secs(86400));
    tester
}

/// Blocks until the earliest wait or timer on the clock is due at `deadline`.
///
/// The advance that reaches the deadline then wakes it, whenever the test makes
/// that advance.
pub fn wait_deadline(tester: &TestClock, deadline: Instant) {
    while tester.next_deadline() != Some(deadline) {
        thread::yield_now();
    }
}

/// Issues a hardware attestation of an identity, signed by the given key at the
/// clock's wall time.
///
/// Signed by the identity itself, it is the placeholder of an Ark that was
/// never onboarded.
pub fn self_attestation(
    signer: &xdsa::SecretKey,
    identity: xdsa::PublicKey,
    clock: &Clock,
) -> Attestation {
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
    let timestamp = clock
        .system_time()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cwt = cwt::issue_at(
        &claims,
        signer,
        CRYPTO_DOMAIN_DEVICE_ATTESTATION,
        timestamp as i64,
    )
    .unwrap();
    Attestation::new(cwt).unwrap()
}

/// Handler of one peer request, with access to the session for reverse
/// requests.
///
/// Returning false ends the peer's session.
pub type Script = Box<dyn FnMut(&Session, Request, Responder) -> bool + Send>;

/// Answers device info requests with a firmware version, unlock requests with a
/// request of the peer's own ahead of the reply, and anything else with the
/// protocol's `UNSUPPORTED` error.
pub fn answering(session: &Session, request: Request, responder: Responder) -> bool {
    let deadline = session.clock().now() + Duration::from_secs(10);
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

/// Returns a script that never answers, holding on to the responders so the
/// wire does not answer for them either.
pub fn silent() -> Script {
    let mut held = Vec::new();
    Box::new(move |_, _, responder| {
        held.push(responder);
        true
    })
}

/// Returns a script that hangs up on the first request, holding on to its
/// responder so only the end of the session reaches the client.
pub fn hangup() -> Script {
    let mut held = Vec::new();
    Box::new(move |_, _, responder| {
        held.push(responder);
        false
    })
}

/// Scripted Ark peer over an in-memory stream.
///
/// It runs until its script finishes or the host closes, and joins its serving
/// thread on drop.
pub struct Peer {
    /// Identity key the peer signs its handshake with.
    pub identity: xdsa::PublicKey,
    /// Host end of the stream, until a test takes it.
    stream: Option<Duplex>,
    /// Serving thread, joined on drop.
    thread: Option<JoinHandle<()>>,
}

impl Peer {
    /// Starts a peer serving the client per the script.
    ///
    /// Both ends of its stream measure their deadlines on the clock.
    pub fn spawn(clock: &Clock, mut script: Script) -> Self {
        // Sign the handshake with a fresh identity attesting itself
        let signer = xdsa::SecretKey::generate();
        let identity = signer.public_key();
        let attestation = self_attestation(&signer, identity.clone(), clock);

        // Serve one session, passing each request to the script
        let (host, ark) = memory::duplex(CAPACITY, clock);
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

    /// Takes the host stream for attachment or forwarding through another
    /// transport.
    ///
    /// # Panics
    ///
    /// Panics if the stream was already taken.
    pub fn stream(&mut self) -> Duplex {
        self.stream.take().expect("stream already taken")
    }

    /// Attaches to the peer using its pinned identity key.
    ///
    /// # Panics
    ///
    /// Panics if the stream was already taken.
    pub fn attach(&mut self) -> Result<(Ark, Identity), Error> {
        let stream = self.stream();
        Ark::attach(
            stream,
            &TrustMode::Recover(Box::new(self.identity.clone())),
            |_| None,
        )
    }
}

impl Drop for Peer {
    /// Releases an unused host stream and joins the peer after its session ends.
    fn drop(&mut self) {
        // An untaken host stream must close before joining the peer's
        // receive loop
        drop(self.stream.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
