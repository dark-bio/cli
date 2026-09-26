// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Pairing rendezvous and opaque companion exchanges.

use super::{
    Services,
    socket::{self, Connection, Socket, socket_mut},
};
use crate::{Error, Timing, schema};
use darkbio_clock::Clock;
use darkbio_crypto::{cbor::Cbor, cose};
use darkbio_wire::protocol::Requester;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tungstenite::Message;

/// Stages of [`Client::pair`](crate::Client::pair), reported on the caller's
/// thread.
///
/// The caller renders the rendezvous, for example as a QR code. Identity and
/// storage messages stay opaque to the host, and the Ark verifies them.
#[derive(Clone, Debug)]
pub enum PairingProgress {
    /// Rendezvous ready to present to the owner for scanning.
    Rendezvous {
        /// Cloud location used by the companion to join this rendezvous.
        colo: String,
        /// Rendezvous secret shared with the companion through the pairing code.
        secret: [u8; 32],
        /// End of the scan window on the connection's clock, when pairing
        /// stops waiting for the companion.
        deadline: Instant,
        /// Pairing encryption key fingerprint, conveyed to the companion out of
        /// band.
        fingerprint: Vec<u8>,
    },
    /// Companion identity received and about to be submitted to the Ark.
    Identity,
    /// Companion storage key received, starting the storage key exchange.
    Storage,
    /// Storage exchange acknowledged, waiting for the owner to approve the
    /// pairing.
    Approval,
    /// Pairing accepted, waiting for the Ark to finish its storage setup.
    Formatting,
}

/// Cloud rendezvous claims read for presentation, without host-side
/// verification.
#[derive(Cbor)]
#[cbor(array)]
struct Rendezvous {
    /// Location where the companion must join the rendezvous.
    colo: String,
    /// Secret carried in the pairing code, never logged by connect.
    secret: [u8; 32],
    /// Cloud scan expiry in Unix seconds, converted once to a monotonic bound.
    deadline: u64,
}

impl Services {
    /// Opens the pairing rendezvous and forwards each opaque exchange between
    /// the companion and the Ark.
    ///
    /// Only the rendezvous authentication can retry. Once pairing starts, a
    /// failure returns to the caller without replaying approvals or storage
    /// changes.
    pub(crate) fn pair(
        &self,
        requester: &Requester,
        timing: Timing,
        progress: impl FnMut(PairingProgress),
    ) -> Result<(), Error> {
        self.sync(requester, timing)?;
        let clock = &self.clock;
        let cloud = self.cloud.as_ref().ok_or(Error::MissingEnvironment)?;

        // Open the rendezvous with a fresh pairing authorization on every attempt
        let (mut socket, fingerprint) = self.authenticate(requester, timing, || {
            let auth = requester
                .request(schema::PairingAuthRequest {}, timing.io(clock))?
                .wait::<schema::PairingAuthResponse>()?;
            let socket = socket::connect(
                cloud,
                &cloud.pairing_url(),
                &auth.auth,
                "Pairing",
                timing.io(clock),
            )?;
            Ok((socket, auth.fprint))
        })?;

        // Forward the exchange, which never retries once it starts
        exchange(requester, timing, &mut socket, fingerprint, progress)?;
        let _ = socket.close(None);
        Ok(())
    }
}

/// Presents the rendezvous, then forwards each opaque exchange between the
/// companion and the Ark.
///
/// The owner's scan waits until the cloud's deadline, which only an earlier
/// absolute deadline of the caller cuts short.
fn exchange(
    requester: &Requester,
    timing: Timing,
    channel: &mut impl Channel,
    fingerprint: Vec<u8>,
    mut progress: impl FnMut(PairingProgress),
) -> Result<(), Error> {
    let clock = &requester.clock();

    // These claims locate the rendezvous and bound scanning. The Ark verifies
    // the companion identity and storage messages forwarded below.
    let rendezvous: Rendezvous = cose::peek(&channel.receive(timing.io(clock))?)
        .map_err(|err| Error::Pairing(err.to_string()))?;
    let expires = scan_deadline(clock, rendezvous.deadline)?;
    let wait = timing.limit(expires);
    progress(PairingProgress::Rendezvous {
        colo: rendezvous.colo,
        secret: rendezvous.secret,
        deadline: wait,
        fingerprint,
    });

    // Wait for the owner's scan, which expires the pairing when the cloud's
    // deadline runs out first
    let identity = channel.receive(wait).map_err(|err| {
        if matches!(err, Error::Timeout) && wait == expires {
            Error::PairingExpired
        } else {
            err
        }
    })?;
    progress(PairingProgress::Identity);
    requester
        .request(
            schema::PairingSetAppIdentityRequest { identity },
            timing.io(clock),
        )?
        .wait::<schema::PairingSetAppIdentityResponse>()?;

    // Exchange storage keys between the companion and the Ark
    let app_key = channel.receive(timing.approval(clock))?;
    progress(PairingProgress::Storage);
    let storage = requester
        .request(
            schema::PairingSetAppStorageRequest { app_key },
            timing.io(clock),
        )?
        .wait::<schema::PairingSetAppStorageResponse>()?;
    channel.send(storage.ark_keys, timing.io(clock))?;
    let app_ack = channel.receive(timing.approval(clock))?;
    requester
        .request(
            schema::PairingAckArkStorageRequest { app_ack },
            timing.io(clock),
        )?
        .wait::<schema::PairingAckArkStorageResponse>()?;

    // Wait for the owner's approval, then pass the Ark's confirmation on
    progress(PairingProgress::Approval);
    let accepted = requester
        .request(
            schema::PairingAcceptanceRequest {},
            timing.window(clock, crate::timing::PAIRING_WINDOW),
        )?
        .wait::<schema::PairingAcceptanceResponse>()?;
    channel.send(accepted.confirm, timing.io(clock))?;

    // Wait for the Ark to set up its storage, then pass that confirmation on
    progress(PairingProgress::Formatting);
    let completed = requester
        .request(
            schema::PairingCompletionRequest {},
            timing.window(clock, crate::timing::PAIRING_WINDOW),
        )?
        .wait::<schema::PairingCompletionResponse>()?;
    channel.send(completed.confirm, timing.io(clock))
}

/// Rendezvous with the companion, carrying opaque payloads both ways, each
/// exchange under its own deadline.
trait Channel {
    /// Waits for one binary payload until the deadline.
    fn receive(&mut self, deadline: Instant) -> Result<Vec<u8>, Error>;

    /// Sends one payload within the deadline.
    fn send(&mut self, bytes: Vec<u8>, deadline: Instant) -> Result<(), Error>;
}

/// Converts the cloud's Unix deadline to an instant on the clock, reading the
/// clock's wall time once.
///
/// A deadline already reached expires the pairing.
fn scan_deadline(clock: &Clock, deadline: u64) -> Result<Instant, Error> {
    let start = clock.now();
    let now = clock
        .system_time()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| Error::Pairing(err.to_string()))?;
    let remaining = Duration::from_secs(deadline)
        .checked_sub(now)
        .filter(|left| !left.is_zero())
        .ok_or(Error::PairingExpired)?;
    start
        .checked_add(remaining)
        .ok_or_else(|| Error::Pairing("invalid pairing deadline".into()))
}

/// Sets the deadline that the pairing socket's next reads and writes run under.
fn bound(socket: &mut Connection, deadline: Instant) {
    let Socket::Blocking {
        deadline: bound, ..
    } = socket_mut(socket)
    else {
        unreachable!("pairing sockets use blocking deadlines")
    };
    *bound = deadline;
}

/// Waits for one binary payload, answering control frames without extending
/// the deadline.
///
/// A close whose reason is `"pairing timed out"` expires the pairing, which
/// keeps the cloud's scan timeout apart from the caller's own deadline.
fn receive(socket: &mut Connection, deadline: Instant) -> Result<Vec<u8>, Error> {
    bound(socket, deadline);
    loop {
        match socket.read().map_err(socket::socket_error)? {
            Message::Binary(bytes) => return Ok(bytes.to_vec()),
            Message::Ping(_) | Message::Pong(_) => {
                socket.flush().map_err(socket::socket_error)?;
            }
            Message::Close(frame) => {
                return Err(match frame {
                    Some(frame) if frame.reason == "pairing timed out" => Error::PairingExpired,
                    Some(frame) => Error::Pairing(format!("pairing rendezvous closed: {frame}")),
                    None => Error::Pairing("pairing rendezvous closed".into()),
                });
            }
            _ => return Err(Error::Pairing("unexpected pairing message".into())),
        }
    }
}

/// Sends one opaque Ark payload to the companion within the deadline.
fn send(socket: &mut Connection, bytes: Vec<u8>, deadline: Instant) -> Result<(), Error> {
    bound(socket, deadline);
    socket
        .send(Message::Binary(bytes.into()))
        .map_err(socket::socket_error)?;
    Ok(())
}

impl Channel for Connection {
    /// Waits on the cloud's pairing socket, answering its control frames.
    fn receive(&mut self, deadline: Instant) -> Result<Vec<u8>, Error> {
        receive(self, deadline)
    }

    /// Sends through the cloud's pairing socket.
    fn send(&mut self, bytes: Vec<u8>, deadline: Instant) -> Result<(), Error> {
        send(self, bytes, deadline)
    }
}

/// Rendezvous deadlines, close reasons and the forwarded pairing exchange.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Ark, TrustMode,
        cloud::{State, http},
        testing::{Peer, test_clock, wait_deadline},
        trust::Realm,
    };
    use darkbio_clock::crossbeam_channel;
    use darkbio_crypto::xdsa;
    use darkbio_wire::protocol::{self, schema::host_to_ark::Content};
    use std::{
        net::TcpListener,
        sync::{Arc, Mutex, mpsc},
        thread,
    };

    /// Budget for loopback I/O that is not exercising expiration.
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// The scan deadline maps from the cloud's Unix time, and only a caller's
    /// absolute deadline cuts it short.
    #[test]
    fn scan_uses_cloud_deadline_and_retains_caller_bound() {
        // Pin wall time to a whole second, so the cloud's deadline maps exactly
        let mut tester = test_clock();
        tester.set_system_time(UNIX_EPOCH + Duration::from_secs(1_789_000_000));
        let clock = tester.clock();

        // A deadline already reached expires the pairing, and a later one maps
        // to the same span on the clock
        assert!(matches!(
            scan_deadline(&clock, 0),
            Err(Error::PairingExpired)
        ));
        assert!(matches!(
            scan_deadline(&clock, 1_789_000_000),
            Err(Error::PairingExpired)
        ));
        let now = clock.now();
        let expiry = scan_deadline(&clock, 1_789_000_060).unwrap();
        assert_eq!(expiry, now + Duration::from_secs(60));

        // The scan keeps its own deadline, cut only by the caller's absolute one
        let timing = Timing::inactivity(Duration::from_millis(1));
        assert_eq!(timing.limit(expiry), expiry);
        let caller = now + Duration::from_secs(1);
        assert_eq!(timing.with_deadline(caller).limit(expiry), caller);
        assert_eq!(Timing::until(expiry + TIMEOUT).limit(expiry), expiry);
    }

    /// A cloud close for the scan timeout expires the pairing, while any other
    /// close reports its reason.
    #[test]
    #[allow(clippy::result_large_err)] // the upgrade callback's error is a whole HTTP response
    fn close_retains_timeout_and_other_reasons() {
        let clock = test_clock().clock();
        for reason in ["pairing timed out", "companion disconnected"] {
            // Accept the upgrade, then close it at once with the reason
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}/pairing", listener.local_addr().unwrap());
            let cloud = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut socket = tungstenite::accept_hdr(
                    stream,
                    |_: &tungstenite::handshake::server::Request,
                     mut response: tungstenite::handshake::server::Response| {
                        response
                            .headers_mut()
                            .insert("Sec-WebSocket-Protocol", "Pairing".parse().unwrap());
                        Ok(response)
                    },
                )
                .unwrap();
                socket
                    .close(Some(tungstenite::protocol::CloseFrame {
                        code: tungstenite::protocol::frame::coding::CloseCode::Normal,
                        reason: reason.into(),
                    }))
                    .unwrap();
            });

            // The next receive maps the close to its error
            let deadline = clock.now() + TIMEOUT;
            let mut socket = socket::connect(
                &http::tests::api(url.clone(), Realm::Hardware, &clock),
                &url,
                &[],
                "Pairing",
                deadline,
            )
            .unwrap();
            let err = receive(&mut socket, deadline).unwrap_err();
            if reason == "pairing timed out" {
                assert!(matches!(err, Error::PairingExpired));
            } else {
                assert!(matches!(err, Error::Pairing(message) if message.contains(reason)));
            }
            cloud.join().unwrap();
        }
    }

    /// Signs a rendezvous at `signed` that expires at `deadline`, both in Unix
    /// seconds.
    fn rendezvous(signed: u64, deadline: u64) -> Vec<u8> {
        let rendezvous = Rendezvous {
            colo: "OTP".into(),
            secret: [9; 32],
            deadline,
        };
        cose::sign_at(
            rendezvous,
            (),
            &xdsa::SecretKey::generate(),
            b"pairing-v1",
            signed as i64,
        )
        .unwrap()
    }

    /// Spawns an Ark peer that answers each pairing request with fixed opaque
    /// payloads and checks the ones it receives.
    ///
    /// With `refuse` set, it refuses the owner's acceptance.
    fn pairing_peer(clock: &Clock, refuse: bool) -> Peer {
        Peer::spawn(
            clock,
            Box::new(move |_, request, responder| {
                let deadline = responder.clock().now() + TIMEOUT;
                match request {
                    Content::PairingAuth(_) => responder.reply(
                        schema::PairingAuthResponse {
                            auth: vec![251, 255],
                            fprint: vec![8; 32],
                        },
                        deadline,
                    ),
                    Content::PairingSetAppId(request) => {
                        assert_eq!(request.identity, [1, 0, 255]);
                        responder.reply(schema::PairingSetAppIdentityResponse {}, deadline)
                    }
                    Content::PairingSetAppStorage(request) => {
                        assert_eq!(request.app_key, [2, 0, 254]);
                        responder.reply(
                            schema::PairingSetAppStorageResponse {
                                ark_keys: vec![3, 0, 253],
                            },
                            deadline,
                        )
                    }
                    Content::PairingAckArkStorage(request) => {
                        assert_eq!(request.app_ack, [4, 0, 252]);
                        responder.reply(schema::PairingAckArkStorageResponse {}, deadline)
                    }
                    Content::PairingAccept(_) if refuse => {
                        responder.fail(schema::Error::new(0x1234, "owner refused"), deadline)
                    }
                    Content::PairingAccept(_) => responder.reply(
                        schema::PairingAcceptanceResponse {
                            confirm: vec![5, 0, 251],
                        },
                        deadline,
                    ),
                    Content::PairingComplete(_) => responder.reply(
                        schema::PairingCompletionResponse {
                            confirm: vec![6, 0, 250],
                        },
                        deadline,
                    ),
                    _ => panic!("unexpected pairing request {request:?}"),
                }
                .unwrap();
                true
            }),
        )
    }

    /// The owner's scan outlives the inactivity allowance until the cloud's
    /// deadline, unless a caller's earlier absolute deadline ends it.
    #[test]
    fn test_scan_waits_under_the_cloud_deadline() {
        /// Companion side of the rendezvous, which the test hands each payload
        /// to.
        ///
        /// Every receive reports its deadline before it waits.
        struct Companion {
            /// Clock the receives wait on.
            clock: Clock,
            /// Payloads the test hands over.
            payloads: crossbeam_channel::Receiver<Vec<u8>>,
            /// Deadline of every receive, in order.
            waits: mpsc::Sender<Instant>,
        }
        impl Channel for Companion {
            /// Reports the deadline, then waits on the clock for the test's
            /// next payload.
            fn receive(&mut self, deadline: Instant) -> Result<Vec<u8>, Error> {
                self.waits.send(deadline).unwrap();
                self.clock
                    .recv_deadline(&self.payloads, deadline)
                    .map_err(|_| Error::Timeout)
            }

            /// Discards the payload, which the test does not check.
            fn send(&mut self, _: Vec<u8>, _: Instant) -> Result<(), Error> {
                Ok(())
            }
        }

        for caller in [None, Some(Duration::from_secs(10))] {
            // Pin wall time to a whole second, so the cloud's deadline maps exactly
            let mut tester = test_clock();
            tester.set_system_time(UNIX_EPOCH + Duration::from_secs(1_789_000_000));
            let clock = tester.clock();
            let start = clock.now();

            // Start pairing with the rendezvous at hand but the companion silent
            let mut peer = pairing_peer(&clock, false);
            let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let (hand, payloads) = crossbeam_channel::unbounded();
            let (waits, receives) = mpsc::channel();
            let (progress, stages) = mpsc::channel();
            hand.send(rendezvous(1_789_000_000, 1_789_000_060)).unwrap();
            let allowance = Timing::inactivity(Duration::from_secs(1));
            let timing = caller.map_or(allowance, |caller| allowance.with_deadline(start + caller));
            let pairing = thread::spawn({
                let requester = session.requester();
                let mut companion = Companion {
                    clock: clock.clone(),
                    payloads,
                    waits,
                };
                move || {
                    exchange(&requester, timing, &mut companion, vec![8; 32], |stage| {
                        progress.send(stage).unwrap()
                    })
                }
            });

            // The rendezvous comes within the allowance, then the scan waits on
            // the cloud's deadline or the caller's earlier one, which is also
            // the deadline the rendezvous is presented with
            assert_eq!(receives.recv().unwrap(), start + Duration::from_secs(1));
            let scan = start + caller.unwrap_or(Duration::from_secs(60));
            assert_eq!(receives.recv().unwrap(), scan);
            assert!(matches!(
                stages.recv().unwrap(),
                PairingProgress::Rendezvous { deadline, .. } if deadline == scan
            ));
            wait_deadline(&tester, scan);

            // Passing the machine allowance leaves the scan waiting
            tester.advance(Duration::from_secs(2));
            assert_eq!(tester.next_deadline(), Some(scan));

            // A late companion completes the exchange, a caller's deadline ends it
            if caller.is_none() {
                for payload in [vec![1, 0, 255], vec![2, 0, 254], vec![4, 0, 252]] {
                    hand.send(payload).unwrap();
                }
                pairing.join().unwrap().unwrap();
            } else {
                tester.advance_to(scan);
                assert!(matches!(pairing.join().unwrap(), Err(Error::Timeout)));
            }
        }
    }

    /// The host relays sealed pairing payloads unchanged and in protocol order,
    /// through completion or the owner's refusal.
    #[test]
    #[allow(clippy::result_large_err)] // the upgrade callback's error is a whole HTTP response
    fn pairing_exchange_and_refusal() {
        // Pin wall time to a whole second, so the cloud's deadline maps exactly
        let mut tester = test_clock();
        tester.set_system_time(UNIX_EPOCH + Duration::from_secs(1_789_000_000));
        let clock = tester.clock();
        let start = clock.now();

        for refuse in [false, true] {
            // Serve the cloud's side of the rendezvous, checking every payload
            // that comes back from the Ark
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let signed = clock
                .system_time()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let expiry = signed + 60;
            let cloud = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                let mut socket = tungstenite::accept_hdr(
                    stream,
                    |request: &tungstenite::handshake::server::Request,
                     mut response: tungstenite::handshake::server::Response| {
                        assert_eq!(request.uri().path(), "/v1/pairing");
                        assert_eq!(
                            request.headers()["Sec-WebSocket-Protocol"],
                            "Pairing, Dark-Auth|-_8"
                        );
                        response
                            .headers_mut()
                            .insert("Sec-WebSocket-Protocol", "Pairing".parse().unwrap());
                        Ok(response)
                    },
                )
                .unwrap();
                socket
                    .send(Message::Binary(rendezvous(signed, expiry).into()))
                    .unwrap();
                socket.send(Message::Ping(vec![9].into())).unwrap();
                socket
                    .send(Message::Binary(vec![1, 0, 255].into()))
                    .unwrap();
                socket
                    .send(Message::Binary(vec![2, 0, 254].into()))
                    .unwrap();
                let mut read = || loop {
                    match socket.read().unwrap() {
                        Message::Binary(bytes) => break bytes.to_vec(),
                        Message::Pong(_) => {}
                        message => panic!("unexpected cloud message {message:?}"),
                    }
                };
                assert_eq!(read(), [3, 0, 253]);
                socket
                    .send(Message::Binary(vec![4, 0, 252].into()))
                    .unwrap();
                if !refuse {
                    assert_eq!(socket.read().unwrap().into_data().as_ref(), [5, 0, 251]);
                    assert_eq!(socket.read().unwrap().into_data().as_ref(), [6, 0, 250]);
                    assert!(matches!(socket.read().unwrap(), Message::Close(_)));
                }
            });

            // Pair an Ark whose cloud setup is already fresh
            let mut peer = pairing_peer(&clock, refuse);
            let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let services = Arc::new(Services {
                clock: clock.clone(),
                cloud: Some(http::tests::api(url, Realm::Hardware, &clock)),
                state: Mutex::new(State {
                    synced: Some((clock.now(), true)),
                    ..Default::default()
                }),
                updating: Mutex::new(()),
            });
            let ark = Ark::start(session, services).unwrap();
            let mut stages = Vec::new();
            let result = ark
                .client()
                .pair(Timing::inactivity(Duration::from_secs(1)), |stage| {
                    stages.push(stage)
                });

            // A refusal ends before formatting, and the first stage presents the
            // cloud's rendezvous either way
            if refuse {
                assert!(
                    matches!(result, Err(Error::Remote(error)) if error.code == 0x1234 && error.msg == "owner refused")
                );
                assert!(
                    !stages
                        .iter()
                        .any(|stage| matches!(stage, PairingProgress::Formatting))
                );
            } else {
                result.unwrap();
                assert!(matches!(stages.last(), Some(PairingProgress::Formatting)));
            }
            assert!(
                matches!(&stages[0], PairingProgress::Rendezvous { colo, secret, deadline, fingerprint }
                if *deadline == start + Duration::from_secs(60) && colo == "OTP" && secret == &[9;32] && fingerprint == &vec![8;32])
            );
            cloud.join().unwrap();
        }
    }
}
