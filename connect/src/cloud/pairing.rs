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
use darkbio_crypto::{cbor::Cbor, cose};
use darkbio_wire::protocol::Requester;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::Message;

/// Pairing stages. The caller renders the rendezvous or turns it into a QR code;
/// identity and storage messages remain opaque and are verified by the Ark.
#[derive(Clone, Debug)]
pub enum PairingProgress {
    /// Rendezvous ready to present to the owner for scanning.
    Rendezvous {
        /// Cloud location used by the companion to join this rendezvous.
        colo: String,
        /// Rendezvous secret shared with the companion through the pairing code.
        secret: [u8; 32],
        /// End of the scan window in Unix seconds.
        deadline: u64,
        /// Pairing encryption key fingerprint conveyed to the companion out of band.
        fingerprint: Vec<u8>,
    },
    /// Companion identity received and about to be submitted to the Ark.
    Identity,
    /// Companion storage key received; storage key exchange is starting.
    Storage,
    /// Storage exchange acknowledged; waiting for physical pairing approval.
    Approval,
    /// Pairing accepted; waiting for the Ark to finish storage setup.
    Formatting,
}

/// Cloud rendezvous claims read for presentation, without host-side verification.
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
    /// Authenticates the rendezvous, then forwards each opaque pairing exchange.
    /// Only initial authentication can retry. Once pairing starts, a failure
    /// returns to the caller without replaying approvals or storage changes.
    pub(crate) fn pair(
        &self,
        requester: &Requester,
        timing: Timing,
        mut progress: impl FnMut(PairingProgress),
    ) -> Result<(), Error> {
        self.sync(requester, timing)?;
        let cloud = self.cloud.as_ref().ok_or(Error::MissingEnvironment)?;
        let (mut socket, fingerprint) = self.authenticate(requester, timing, || {
            let auth = requester
                .request(schema::PairingAuthRequest {}, timing.io())?
                .wait::<schema::PairingAuthResponse>()?;
            let socket = socket::connect(
                cloud,
                &cloud.pairing_url(),
                &auth.auth,
                "Pairing",
                timing.io(),
            )?;
            Ok((socket, auth.fprint))
        })?;
        // These claims locate the rendezvous and bound scanning. The Ark verifies
        // the companion identity and storage messages forwarded below.
        let rendezvous: Rendezvous = cose::peek(&receive(&mut socket, timing.io())?)
            .map_err(|err| Error::Pairing(err.to_string()))?;
        let expires = scan_deadline(rendezvous.deadline)?;
        progress(PairingProgress::Rendezvous {
            colo: rendezvous.colo,
            secret: rendezvous.secret,
            deadline: rendezvous.deadline,
            fingerprint,
        });
        let wait = timing.limit(expires);
        let identity = receive(&mut socket, wait).map_err(|err| {
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
                timing.io(),
            )?
            .wait::<schema::PairingSetAppIdentityResponse>()?;
        let app_key = receive(&mut socket, timing.approval())?;
        progress(PairingProgress::Storage);
        let storage = requester
            .request(schema::PairingSetAppStorageRequest { app_key }, timing.io())?
            .wait::<schema::PairingSetAppStorageResponse>()?;
        send(&mut socket, storage.ark_keys, timing.io())?;
        let app_ack = receive(&mut socket, timing.approval())?;
        requester
            .request(schema::PairingAckArkStorageRequest { app_ack }, timing.io())?
            .wait::<schema::PairingAckArkStorageResponse>()?;
        progress(PairingProgress::Approval);
        let accepted = requester
            .request(
                schema::PairingAcceptanceRequest {},
                timing.window(crate::timing::PAIRING_WINDOW),
            )?
            .wait::<schema::PairingAcceptanceResponse>()?;
        send(&mut socket, accepted.confirm, timing.io())?;
        progress(PairingProgress::Formatting);
        let completed = requester
            .request(
                schema::PairingCompletionRequest {},
                timing.window(crate::timing::PAIRING_WINDOW),
            )?
            .wait::<schema::PairingCompletionResponse>()?;
        send(&mut socket, completed.confirm, timing.io())?;
        let _ = socket.close(None);
        Ok(())
    }
}

/// Converts the cloud's Unix deadline once, then waits on the monotonic clock.
fn scan_deadline(deadline: u64) -> Result<Instant, Error> {
    let start = Instant::now();
    let now = SystemTime::now()
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

/// Changes the bound before the next opaque exchange.
fn bound(socket: &mut Connection, deadline: Instant) {
    let Socket::Blocking {
        deadline: bound, ..
    } = socket_mut(socket)
    else {
        unreachable!("pairing sockets use blocking deadlines")
    };
    *bound = deadline;
}

/// Waits for one binary payload, answering control frames without renewing time.
/// The cloud's scan timeout remains distinct from a caller's earlier deadline.
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

/// Sends one opaque Ark reply before advancing to the next pairing stage.
fn send(socket: &mut Connection, bytes: Vec<u8>, deadline: Instant) -> Result<(), Error> {
    bound(socket, deadline);
    socket
        .send(Message::Binary(bytes.into()))
        .map_err(socket::socket_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Ark, TrustMode,
        cloud::{State, http},
        testing::Peer,
        trust::Realm,
    };
    use darkbio_crypto::xdsa;
    use darkbio_wire::protocol::{self, schema::host_to_ark::Content};
    use std::{
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };
    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn scan_uses_cloud_deadline_and_retains_caller_bound() {
        assert!(matches!(scan_deadline(0), Err(Error::PairingExpired)));
        let now = Instant::now();
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiry = scan_deadline(epoch + 60).unwrap();
        assert!(expiry > now + Duration::from_secs(58));
        assert!(expiry <= now + Duration::from_secs(60));
        let timing = Timing::inactivity(Duration::from_millis(1));
        assert_eq!(timing.limit(expiry), expiry);
        let caller = now + Duration::from_secs(1);
        assert_eq!(timing.with_deadline(caller).limit(expiry), caller);
        assert_eq!(Timing::until(expiry + TIMEOUT).limit(expiry), expiry);
    }

    #[test]
    #[allow(clippy::result_large_err)] // Tungstenite's HTTP callback owns its rejection.
    fn close_retains_timeout_and_other_reasons() {
        for reason in ["pairing timed out", "companion disconnected"] {
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
            let deadline = Instant::now() + TIMEOUT;
            let mut socket = socket::connect(
                &http::tests::api(url.clone(), Realm::Hardware),
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

    /// The host only relays sealed payloads. Both completion messages and a
    /// refusal retain their protocol ordering; a scan can outlive an I/O wait.
    #[test]
    #[allow(clippy::result_large_err)] // Tungstenite requires a full HTTP rejection response.
    fn pairing_exchange_and_refusal() {
        for refuse in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let expiry = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 60;
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
                let rendezvous = Rendezvous {
                    colo: "OTP".into(),
                    secret: [9; 32],
                    deadline: expiry,
                };
                let signed =
                    cose::sign(rendezvous, (), &xdsa::SecretKey::generate(), b"pairing-v1")
                        .unwrap();
                socket.send(Message::Binary(signed.into())).unwrap();
                thread::sleep(Duration::from_millis(1200));
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
            let mut peer = Peer::spawn(Box::new(move |_, request, responder| {
                let deadline = Instant::now() + TIMEOUT;
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
            }));
            let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let services = Arc::new(Services {
                cloud: Some(http::tests::api(url, Realm::Hardware)),
                state: Mutex::new(State {
                    synced: Some((Instant::now(), true)),
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
                if *deadline == expiry && colo == "OTP" && secret == &[9;32] && fingerprint == &vec![8;32])
            );
            cloud.join().unwrap();
        }
    }
}
