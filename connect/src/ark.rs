// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Session ownership and typed request handles.

use crate::cloud::Services;
use crate::incoming::{self, Incoming};
use crate::{
    Dataset, Error, ExecutionProgress, Firmware, Identity, Registration, Request, Setup, Timing,
    UpdateProgress, UploadProgress, dataset, execution,
};
use darkbio_clock::Clock;
use darkbio_wire::protocol::{self, Message, Promise, Requester, Responder, Session, schema};
use darkbio_wire::transport::{self, Verifier};
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Owner of a connection to an Ark, whose closing or dropping ends the session.
///
/// Ending the session also ends the requests issued through its [`Client`]
/// handles. Relay traffic is dispatched internally when a cloud is selected.
/// Other requests wait in a bounded queue for [`Self::recv`] while clients
/// issue outgoing calls.
pub struct Ark {
    /// Request handle of the wire session owned by the dispatcher.
    requester: Requester,
    /// Handle that stops wire, cloud setup and application receives together.
    closer: Closer,
    /// Requests not claimed by cloud services.
    incoming: Arc<Incoming>,
    /// Lazy prerequisites shared by all request handles of this session.
    services: Arc<Services>,
}

impl Ark {
    /// Installs the cloud credentials provider for this session's clients.
    ///
    /// Install it before using the clients. The provider serves every cloud
    /// request and reconnect, and status requests never call it. A connection
    /// without a cloud route ignores it.
    pub fn set_cloud_auth(&mut self, auth: impl crate::CloudAuth + 'static) {
        self.services.set_cloud_auth(Arc::new(auth));
    }

    /// Authenticates the peer under wire's handshake timeout, then selects cloud
    /// routing for the session.
    ///
    /// It returns the verifier's identity information. Every operation of the
    /// connection reads time from the stream's clock. Failure closes the stream.
    pub(crate) fn attach<R, W, V>(
        stream: transport::Stream<R, W>,
        verifier: &V,
        cloud: impl FnOnce(&Identity) -> Option<(crate::trust::Environment, crate::trust::Realm)>,
    ) -> Result<(Self, V::Info), Error>
    where
        R: transport::Read + Send + 'static,
        W: transport::Write + Send + 'static,
        V: Verifier<Info = Identity>,
    {
        // Take the stream's clock before the handshake consumes the stream
        let clock = stream.clock();

        // Authenticate the peer, stream I/O running out of time being a timeout
        let (session, info) = protocol::connect(stream, verifier).map_err(|err| {
            if let protocol::Error::Transport(cause) = &err
                && let transport::Error::RecvFailed(io) | transport::Error::SendFailed(io) =
                    &**cause
                && matches!(
                    io.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                )
            {
                return Error::Timeout;
            }
            Error::Handshake(err)
        })?;

        // Route cloud traffic by the authenticated identity, then start dispatch
        let services = Arc::new(Services::new(&info, cloud(&info), &clock));
        Ok((Self::start(session, services)?, info))
    }

    /// Starts dispatch for an authenticated session and its cloud services.
    pub(crate) fn start(session: Session, services: Arc<Services>) -> Result<Self, Error> {
        let incoming = Arc::new(Incoming::default());
        let ark = Self {
            requester: session.requester(),
            closer: Closer {
                wire: session.closer(),
                services: services.clone(),
                incoming: incoming.clone(),
            },
            incoming: incoming.clone(),
            services: services.clone(),
        };
        std::thread::Builder::new()
            .name("ark-dispatch".into())
            .spawn(move || incoming::dispatch(session, services, incoming))
            .map_err(Error::Worker)?;
        Ok(ark)
    }

    /// Returns a clonable request handle bound to this session.
    ///
    /// The handle does not keep the connection open.
    pub fn client(&self) -> Client {
        Client {
            requester: self.requester.clone(),
            services: self.services.clone(),
        }
    }

    /// Blocks for a request not handled by cloud services, or returns the
    /// session's ending reason.
    ///
    /// Closing discards any queued requests. Wire answers unknown request types
    /// automatically. The responder keeps wire's reply completion and automatic
    /// reply semantics.
    pub fn recv(&mut self) -> Result<(schema::ark_to_host::Content, Responder), Error> {
        Ok(self.incoming.recv()?)
    }

    /// Returns a handle for closing the session from another thread, including
    /// while its owner is blocked in [`Self::recv`].
    pub fn closer(&self) -> Closer {
        self.closer.clone()
    }

    /// Closes the session, wakes blocked receives and fails pending requests.
    ///
    /// It does not join application handlers or wait for the peer to observe
    /// closure.
    pub fn close(&self) {
        self.closer().close();
    }
}

impl Drop for Ark {
    /// Ends setup waits along with the owned wire session.
    fn drop(&mut self) {
        self.close();
    }
}

/// Clonable handle for closing the connection and waking callers waiting for
/// setup.
///
/// Holding the handle does not keep the Ark session open.
#[derive(Clone, Debug)]
pub struct Closer {
    /// Closer of the original wire session.
    wire: protocol::Closer,
    /// Cloud setup state, whose prerequisite waits end with the session.
    services: Arc<Services>,
    /// Application queue, whose receives wake when the session closes.
    incoming: Arc<Incoming>,
}

impl Closer {
    /// Closes the original connection and its relay.
    ///
    /// Setup I/O already in progress keeps its deadline; setup waiters are
    /// released immediately.
    pub fn close(&self) {
        self.wire.close();
        self.services.close();
        self.incoming.close(protocol::Error::Closed);
    }
}

/// Clonable handle for issuing typed requests through its original session.
///
/// Each request carries its own deadline. Handles do not keep the session open.
#[derive(Clone, Debug)]
pub struct Client {
    /// Wire request handle bound to the original session.
    requester: Requester,
    /// Prerequisite state shared with the owner and the other clients.
    services: Arc<Services>,
}

impl Client {
    /// Returns the clock of this client's connection.
    ///
    /// Every deadline passed to the client is measured on it, so callers build
    /// their deadlines from it.
    pub fn clock(&self) -> Clock {
        self.services.clock().clone()
    }

    /// Sends a request and waits for its typed response under the chosen timing.
    ///
    /// An absolute deadline covers setup, queueing, sending and accepting the
    /// response; decoding is outside it. Reuse it to bound several calls. An
    /// inactivity allowance is renewed for each prerequisite and the request.
    /// Expiration does not cancel an operation the Ark has already received.
    ///
    /// A reserved `UNAVAILABLE` refusal is retried once after sync, only if the
    /// device reports lost cloud setup. Other refusals are returned unchanged.
    pub fn call<R: Request>(
        &self,
        request: R,
        timing: impl Into<Timing>,
    ) -> Result<R::Response, Error> {
        let timing = timing.into();
        let request = request.into();

        // A request needing no cloud setup has no lost setup to retry
        if matches!(R::SETUP, Setup::None) {
            return self.send_message::<R>(request, timing)?.wait();
        }

        // Keep a copy of the request for one retry after a reported setup loss
        let result = self.send_message::<R>(request.clone(), timing)?.wait();
        if matches!(&result, Err(Error::Remote(error)) if error.code == schema::ReservedErrors::Unavailable as u64)
            && !self.services.synced(&self.requester, timing)?
        {
            // UNAVAILABLE refused the request before serving it. Only a reported
            // loss of cloud setup permits one retry, never an application error.
            self.sync(timing)?;
            return self.send_message::<R>(request, timing)?.wait();
        }
        result
    }

    /// Sends a request and waits with a budget starting now.
    ///
    /// Use [`Self::call`] with one deadline when several requests must share a
    /// budget.
    pub fn call_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<R::Response, Error> {
        let deadline = self
            .services
            .clock()
            .now()
            .checked_add(timeout)
            .ok_or(Error::Timeout)?;
        self.call(request, deadline)
    }

    /// Establishes the request's prerequisites, then queues it without waiting
    /// for output or a response.
    ///
    /// The first cloud-dependent send may wait for sync and relay attachment,
    /// according to the request's prerequisites. Unlike [`Self::call`], a
    /// refusal is returned without retrying lost setup. Waiting on the promise
    /// does not refresh the deadline; dropping it does not cancel the request.
    /// Wire's output queue has no capacity limit, so the caller bounds the
    /// number of outstanding requests.
    pub fn send<R: Request>(
        &self,
        request: R,
        timing: impl Into<Timing>,
    ) -> Result<Pending<R::Response>, Error> {
        self.send_message::<R>(request.into(), timing.into())
    }

    /// Establishes typed prerequisites before starting the wire response window.
    ///
    /// A device info request keeps its request time, so its answer can inform
    /// the lazy cloud setup.
    fn send_message<R: Request>(
        &self,
        request: Message,
        timing: Timing,
    ) -> Result<Pending<R::Response>, Error> {
        tracing::debug!("sending request {}", std::any::type_name::<R>());

        // Establish the prerequisites the request type needs
        match R::SETUP {
            Setup::None => {}
            Setup::Cloud => self.services.sync(&self.requester, timing)?,
            Setup::Relay => self.services.relay(&self.requester, timing)?,
        }

        // Bound the response by its protocol window or the inactivity allowance
        let clock = self.services.clock();
        let deadline =
            R::WINDOW.map_or_else(|| timing.io(clock), |window| timing.window(clock, window));

        // Queue the request, noting when a device info query was sent
        let setup = matches!(request, Message::DeviceInfoRequest(_))
            .then(|| (self.services.clone(), clock.now()));
        let promise = self
            .requester
            .request(request, deadline)
            .map_err(|error| self.services.wire_error(error))?;
        Ok(Pending {
            promise,
            setup,
            response: PhantomData,
        })
    }

    /// Establishes prerequisites and queues a request with a budget starting now.
    ///
    /// Waiting on the returned promise keeps that deadline, even if done later.
    pub fn send_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<Pending<R::Response>, Error> {
        let deadline = self
            .services
            .clock()
            .now()
            .checked_add(timeout)
            .ok_or(Error::Timeout)?;
        self.send(request, deadline)
    }

    /// Checks the cloud registry for this Ark, synchronizing first if necessary.
    ///
    /// Setup, proof generation and HTTP share the supplied deadline. A refused
    /// proof triggers one refresh and a retry with a new proof. The returned
    /// registration may be inactive; its flags explain why.
    pub fn genuine(&self, timing: impl Into<Timing>) -> Result<Registration, Error> {
        self.services.genuine(&self.requester, timing.into())
    }

    /// Authorizes, streams, verifies and installs firmware under the chosen
    /// timing.
    ///
    /// Reads and callbacks run on this thread and must return promptly; the
    /// deadline cannot interrupt a blocking reader. Success acknowledges
    /// installation; the Ark then reboots, and this call does not verify the
    /// subsequent boot. Failed updates are never replayed automatically. A
    /// concurrent update through another clone fails at once, and raw firmware
    /// requests must not run concurrently with this operation.
    ///
    /// A cloud proof rejection refreshes keys, then returns
    /// [`Error::ProofRejected`] so the caller can start a new attempt with a new
    /// approval.
    pub fn update_firmware(
        &self,
        firmware: &Firmware,
        reader: &mut impl io::Read,
        timing: impl Into<Timing>,
        progress: impl FnMut(UpdateProgress),
    ) -> Result<(), Error> {
        self.services
            .update_firmware(&self.requester, firmware, reader, timing.into(), progress)
    }

    /// Pairs an unpaired Ark through the cloud rendezvous.
    ///
    /// The caller presents the rendezvous to the owner; the Ark authenticates
    /// every opaque exchange. Authentication may refresh cloud keys and retry
    /// once. The pairing exchange itself is never retried automatically.
    /// Progress callbacks run on this thread and must return promptly so the
    /// rendezvous and approval windows can be serviced.
    pub fn pair(
        &self,
        timing: impl Into<Timing>,
        progress: impl FnMut(crate::PairingProgress),
    ) -> Result<(), Error> {
        self.services.pair(&self.requester, timing.into(), progress)
    }

    /// Refreshes the cloud keys and signed clock explicitly.
    ///
    /// Other requests synchronize lazily, reusing the Ark's reported setup while
    /// fresh.
    pub fn sync(&self, timing: impl Into<Timing>) -> Result<(), Error> {
        self.services.resync(&self.requester, timing.into())
    }

    /// Attaches the companion relay after sync, reusing a connected attachment.
    pub fn attach_relay(&self, timing: impl Into<Timing>) -> Result<(), Error> {
        self.services.relay(&self.requester, timing.into())
    }

    /// Identifies a dataset from its first 1 MiB without opening an upload
    /// session.
    ///
    /// It consumes that prefix from the reader; rewind it before uploading. A
    /// source shorter than the prefix is an error. The reader must enforce its
    /// own read timeout; the operation deadline is checked before and after
    /// reading.
    pub fn identify_dataset(
        &self,
        name: &str,
        size: u64,
        reader: &mut impl io::Read,
        timing: impl Into<Timing>,
    ) -> Result<schema::SlotIdentifyResponse, Error> {
        let timing = timing.into();
        let clock = self.services.clock();
        timing.check(clock)?;

        // Read the prefix, a timed out source read being a timeout
        let mut chunk = vec![0; size.min(1024 * 1024) as usize];
        reader.read_exact(&mut chunk).map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                Error::Timeout
            } else {
                Error::DatasetRead(error)
            }
        })?;
        timing.check(clock)?;

        // Ask the Ark to identify the prefix as any dataset kind
        self.call(
            schema::SlotIdentifyRequest {
                name: name.into(),
                size,
                chunk,
                kinds: Vec::new(),
            },
            timing,
        )
    }

    /// Streams exactly the declared bytes and waits for processing.
    ///
    /// The Ark identifies the target when no slot is supplied. An expected
    /// SHA-256 is checked before processing; no URLs, caching or retries are
    /// involved. Reads and progress callbacks run on this thread and must
    /// return promptly. Failures attempt cancellation without replacing the
    /// original error.
    pub fn upload_dataset(
        &self,
        dataset: &Dataset,
        reader: &mut impl io::Read,
        timing: impl Into<Timing>,
        progress: impl FnMut(UploadProgress),
    ) -> Result<(), Error> {
        let timing = timing.into();
        self.services.sync(&self.requester, timing)?;
        dataset::upload(&self.requester, dataset, reader, timing, progress)
    }

    /// Uploads an app, obtains companion approval and waits for its result.
    ///
    /// The source must contain exactly `size` bytes. The Ark validates the app
    /// and its dataset requirements. An unsuccessful app still returns its
    /// result; inspect its `success` flag. Output is kept as the Ark returns it,
    /// and a failed app's streams come back only when its manifest sets
    /// `develop`.
    ///
    /// Setup, upload, approval and execution share the deadline. Reads and
    /// progress callbacks run on this thread and must return promptly. The
    /// deadline cannot interrupt a blocking reader. Failed operations attempt
    /// cancellation within the remaining time, at most 1 s, and are never
    /// retried.
    ///
    /// [`ExecutionProgress::Started`] supplies the task ID for cancellation with
    /// [`schema::ExecutionCancelRequest`] through another client clone. Closing
    /// the connection sends no cancellation. Retrieving a completed result
    /// consumes it, so only one caller should poll a task's status.
    pub fn execute(
        &self,
        size: u64,
        reader: &mut impl io::Read,
        timing: impl Into<Timing>,
        progress: impl FnMut(ExecutionProgress),
    ) -> Result<schema::ExecutionResultResponse, Error> {
        let timing = timing.into();
        self.services.sync(&self.requester, timing)?;
        execution::execute(&self.requester, size, reader, timing, progress, |taskid| {
            self.call(schema::ExecutionScheduleRequest { taskid }, timing)
                .map(drop)
        })
    }
}

/// Result of a typed request, decoded when [`Self::wait`] takes the response.
///
/// Dropping it discards the result without cancelling the request.
#[derive(Debug)]
pub struct Pending<T> {
    /// Encoded response and the notification registered for its completion.
    promise: Promise<Message>,
    /// Setup state and request time of a device info query, whose answer
    /// informs this connection's lazy setup.
    setup: Option<(Arc<Services>, Instant)>,
    /// Response type selected by the request, without owning a value of it.
    response: PhantomData<fn() -> T>,
}

impl<T> Pending<T> {
    /// Sends an event when the request completes, successfully or with an error.
    ///
    /// One channel can observe many requests. Registration leaves the response
    /// encoded until [`Self::wait`] and does not change its deadline.
    ///
    /// # Panics
    ///
    /// Panics if a notification was already registered on this promise.
    pub fn notify<E: Copy + Send + 'static>(&mut self, sender: mpsc::Sender<E>, event: E) {
        self.promise.notify(move || {
            let _ = sender.send(event);
        });
    }
}

impl<T: TryFrom<Message, Error = protocol::Error>> Pending<T> {
    /// Waits for completion and decodes the expected response.
    ///
    /// An accepted response remains available after its deadline or the
    /// session's closure.
    pub fn wait(self) -> Result<T, Error> {
        let message = self.promise.wait::<Message>()?;
        if let Some((services, requested)) = self.setup
            && let Message::DeviceInfoResponse(info) = &message
        {
            services.reported(info, requested);
        }
        Ok(T::try_from(message)?)
    }
}

/// Request ownership, completion and bidirectional protocol regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        DeviceInfoRequest, OnboardingRequest, RelayAppToArkResponse, RelayArkToAppRequest,
        UnlockRequest, UnlockResponse,
    };
    use crate::testing::{Peer, answering, hangup, silent, test_clock, wait_deadline};
    use std::thread;

    /// Budget for test I/O that is not exercising expiration.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Cloud selection sees the authenticated identity and keeps its session.
    #[test]
    fn test_cloud_selection_after_handshake() {
        use crate::trust::{Environment, Realm};

        /// Verifier reporting every peer it accepts as attested under a fixed
        /// environment.
        struct Attested(Environment);

        impl Verifier for Attested {
            type Info = Identity;

            fn verify(
                &self,
                attestation: &transport::Attestation,
                now: std::time::SystemTime,
            ) -> Result<(crate::wire::crypto::xdsa::PublicKey, Identity), String> {
                let (key, _) = crate::TrustMode::RootOrSelf.verify(attestation, now)?;
                let identity = Identity::Attested {
                    env: self.0,
                    device: crate::trust::device::Device {
                        realm: Realm::Hardware,
                        identity: key.clone(),
                        serial: "test-device".into(),
                        oem: crate::wire::crypto::cwt::claims::eat::Oemid::new_pen(0),
                        model: vec![],
                        version: String::new(),
                        issued: 0,
                        expiry: None,
                    },
                };
                Ok((key, identity))
            }
        }

        // Each environment's attested identity reaches the selector once, and
        // the session it routes still serves requests
        let clock = test_clock().clock();
        for &env in crate::identity::ENVIRONMENTS {
            let mut peer = Peer::spawn(&clock, Box::new(answering));
            let mut selected = None;
            let (ark, identity) = Ark::attach(peer.stream(), &Attested(env), |identity| {
                let Identity::Attested { env, device } = identity else {
                    panic!("expected attested identity");
                };
                assert_eq!(device.identity.to_bytes(), peer.identity.to_bytes());
                assert!(selected.replace(*env).is_none());
                Some((*env, device.realm))
            })
            .unwrap();
            assert_eq!(selected, Some(env));
            assert!(matches!(identity, Identity::Attested { env: actual, .. } if actual == env));
            assert_eq!(
                ark.client()
                    .call(DeviceInfoRequest {}, clock.now() + TIMEOUT)
                    .unwrap()
                    .firmware_version,
                "1.0.0"
            );
        }
    }

    /// Failed authentication never invokes the cloud selector.
    #[test]
    fn test_cloud_selection_rejects_failed_handshake() {
        let mut peer = Peer::spawn(&test_clock().clock(), Box::new(answering));
        let wrong = crate::wire::crypto::xdsa::SecretKey::generate().public_key();
        let result = Ark::attach(
            peer.stream(),
            &crate::TrustMode::Recover(Box::new(wrong)),
            |_| panic!("cloud selected before authentication"),
        );
        assert!(matches!(result, Err(Error::Handshake(_))));
    }

    /// Unlock request needing no setup, for exercising reverse requests without
    /// automatic cloud attachment.
    struct ManualUnlock;

    impl From<ManualUnlock> for Message {
        /// Encodes an unlock while leaving setup to the test's reverse handler.
        fn from(_: ManualUnlock) -> Self {
            UnlockRequest {}.into()
        }
    }

    impl Request for ManualUnlock {
        type Response = UnlockResponse;
        const SETUP: Setup = Setup::None;
    }

    /// Application error codes reach the peer, and a reserved refusal leaves the
    /// session available for subsequent requests.
    #[test]
    fn test_coded_errors() {
        /// Application refusal returned by the host's companion handler.
        #[derive(Debug, thiserror::Error)]
        #[error("companion rejected authorization")]
        struct Denied;

        impl crate::CodedError for Denied {
            /// Uses an application code outside wire's reserved error range.
            fn code(&self) -> u64 {
                0x100
            }
        }

        // The peer relays each unlock to the host, then refuses it as unavailable
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(
            &clock,
            Box::new(|session, request, responder| {
                if !matches!(request, schema::host_to_ark::Content::Unlock(_)) {
                    return answering(session, request, responder);
                }
                let deadline = session.clock().now() + TIMEOUT;
                let error = session
                    .requester()
                    .request(RelayArkToAppRequest::default(), deadline)
                    .unwrap()
                    .wait::<RelayAppToArkResponse>()
                    .unwrap_err();
                assert!(matches!(
                    error, protocol::Error::Remote(error)
                        if error.code == 0x100 && error.msg == "companion rejected authorization"
                ));
                responder
                    .fail(
                        schema::Error::reserved(
                            schema::ReservedErrors::Unavailable,
                            "authorization required",
                        ),
                        deadline,
                    )
                    .unwrap()
                    .wait()
                    .unwrap();
                true
            }),
        );

        // The host refuses the relayed request with its own application error
        let (mut ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client.send(ManualUnlock, clock.now() + TIMEOUT).unwrap();
        let (request, responder) = ark.recv().unwrap();
        assert!(matches!(request, schema::ark_to_host::Content::RelayReq(_)));
        responder
            .fail(Denied, clock.now() + TIMEOUT)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            pending.wait(), Err(Error::Remote(error))
                if error.code == schema::ReservedErrors::Unavailable as u64
        ));

        // The refused unlock leaves the session open for later requests
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, clock.now() + TIMEOUT)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
    }

    /// Wire answers unknown content automatically while known requests continue
    /// through the host's receive loop.
    #[test]
    fn test_unknown_requests() {
        use crate::testing::self_attestation;
        use darkbio_crypto::xdsa;
        use darkbio_wire::{memory, prost, transport};
        use prost::Message as _;

        /// Future Ark envelope carrying content absent from the current wire schema.
        #[derive(prost::Message)]
        #[prost(prost_path = "darkbio_wire::prost")]
        struct FutureRequest {
            /// Correlation ID echoed by wire's automatic refusal.
            #[prost(uint64, tag = "1")]
            id: u64,
            /// Payload under a tag unknown to the current schema.
            #[prost(bytes = "vec", tag = "2047")]
            content: Vec<u8>,
        }

        // A raw peer sends unknown and known requests, then collects both replies
        let clock = test_clock().clock();
        let signer = xdsa::SecretKey::generate();
        let identity = signer.public_key();
        let attestation = self_attestation(&signer, identity.clone(), &clock);
        let (host, remote) = memory::duplex(256 * 1024, &clock);
        let peer = thread::spawn(move || {
            let mut server = transport::Server::new(remote, signer, attestation);
            let transport::Event::Connected(sender) = server.recv().unwrap() else {
                panic!("expected handshake");
            };
            sender
                .send(
                    &FutureRequest {
                        id: 2,
                        content: vec![42],
                    }
                    .encode_to_vec(),
                )
                .unwrap();
            sender
                .send(
                    &schema::ArkToHost {
                        id: 4,
                        err: None,
                        content: Some(schema::ark_to_host::Content::DeviceInfo(Default::default())),
                    }
                    .encode_to_vec(),
                )
                .unwrap();
            let mut replies = std::collections::BTreeMap::new();
            for _ in 0..2 {
                let transport::Event::Message(bytes) = server.recv().unwrap() else {
                    panic!("expected error reply");
                };
                let reply = schema::HostToArk::decode(bytes.as_slice()).unwrap();
                assert!(reply.content.is_none());
                replies.insert(reply.id, reply.err.unwrap().code);
            }
            replies
        });

        // The host refuses the known request itself, as unsupported
        let (mut ark, _) =
            Ark::attach(host, &crate::TrustMode::Recover(Box::new(identity)), |_| {
                None
            })
            .unwrap();
        let (request, responder) = ark.recv().unwrap();
        assert!(matches!(
            request,
            schema::ark_to_host::Content::DeviceInfo(_)
        ));
        responder
            .fail(
                schema::Error::reserved(
                    schema::ReservedErrors::Unsupported,
                    "host does not serve device info",
                ),
                clock.now() + TIMEOUT,
            )
            .unwrap()
            .wait()
            .unwrap();

        // Both replies have been written. Close before joining so a missing reply
        // causes a peer EOF rather than leaving this test waiting indefinitely.
        ark.close();
        let replies = peer.join().unwrap();
        assert_eq!(
            replies.get(&2),
            Some(&(schema::ReservedErrors::Unknown as u64))
        );
        assert_eq!(
            replies.get(&4),
            Some(&(schema::ReservedErrors::Unsupported as u64))
        );
    }

    /// Concurrent typed requests keep their responses and completion tokens, and
    /// a reserved refusal returns through the same interface.
    #[test]
    fn test_requests() {
        // A notified request signals its token before its response is taken
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let (completed, events) = mpsc::channel();
        let mut pending = client
            .send(DeviceInfoRequest {}, clock.now() + TIMEOUT)
            .unwrap();
        pending.notify(completed, 7);
        assert_eq!(events.recv().unwrap(), 7);
        assert_eq!(pending.wait().unwrap().firmware_version, "1.0.0");

        // Concurrent callers sharing one deadline each get their own response
        let deadline = clock.now() + TIMEOUT;
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                thread::spawn(move || {
                    client
                        .call(DeviceInfoRequest {}, deadline)
                        .unwrap()
                        .firmware_version
                })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), "1.0.0");
        }

        // A reserved refusal comes back as a remote error
        assert!(
            matches!(client.call(OnboardingRequest::default(), clock.now() + TIMEOUT), Err(Error::Remote(error)) if error.code == schema::ReservedErrors::Unsupported as u64)
        );
    }

    /// Dropping the owner closes pending requests and refuses surviving clients.
    #[test]
    fn test_owner_drop() {
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, silent());
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client
            .send(DeviceInfoRequest {}, clock.now() + TIMEOUT)
            .unwrap();
        drop(ark);
        assert!(matches!(pending.wait(), Err(Error::Closed)));
        assert!(matches!(
            client.call(DeviceInfoRequest {}, clock.now() + TIMEOUT),
            Err(Error::Closed)
        ));
    }

    /// A closer wakes both the receive loop and outstanding requests.
    #[test]
    fn test_close() {
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, silent());
        let (mut ark, _) = peer.attach().unwrap();
        let pending = ark
            .client()
            .send(DeviceInfoRequest {}, clock.now() + TIMEOUT)
            .unwrap();
        let closer = ark.closer();
        let receive = thread::spawn(move || ark.recv());
        closer.close();
        assert!(matches!(receive.join().unwrap(), Err(Error::Closed)));
        assert!(matches!(pending.wait(), Err(Error::Closed)));
    }

    /// A remote disconnect retains its reason for receives and later requests.
    #[test]
    fn test_disconnect() {
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, hangup());
        let (mut ark, _) = peer.attach().unwrap();
        assert!(matches!(
            ark.client()
                .call(DeviceInfoRequest {}, clock.now() + TIMEOUT),
            Err(Error::Disconnected(_))
        ));
        assert!(matches!(ark.recv(), Err(Error::Disconnected(_))));
        assert!(matches!(
            ark.client()
                .call(DeviceInfoRequest {}, clock.now() + TIMEOUT),
            Err(Error::Disconnected(_))
        ));
    }

    /// A short request timeout leaves a concurrent request's budget intact.
    #[test]
    fn test_timeouts() {
        // Leave a request outstanding under the long budget
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut peer = Peer::spawn(&clock, silent());
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client.send_timeout(DeviceInfoRequest {}, TIMEOUT).unwrap();

        // The short call's deadline is the earliest one, and reaching it
        // expires the call
        let deadline = clock.now() + Duration::from_millis(20);
        let short = thread::spawn({
            let client = client.clone();
            move || client.call_timeout(DeviceInfoRequest {}, Duration::from_millis(20))
        });
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        assert!(matches!(short.join().unwrap(), Err(Error::Timeout)));

        // The longer request is still pending until the owner closes
        ark.close();
        assert!(matches!(pending.wait(), Err(Error::Closed)));
    }

    /// Reusing a deadline across calls and cloned handles does not renew its budget.
    #[test]
    fn test_deadlines() {
        // A call within the shared deadline succeeds
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let deadline = clock.now() + Duration::from_secs(1);
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, deadline)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );

        // Spend the rest of the budget, which a cloned handle cannot renew
        tester.advance_to(deadline);
        assert!(matches!(
            client.clone().call(DeviceInfoRequest {}, deadline),
            Err(Error::Timeout)
        ));

        // A budget starting now still serves a call
        assert_eq!(
            client
                .call_timeout(DeviceInfoRequest {}, TIMEOUT)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
    }

    /// An unlock can wait for the host to return an opaque companion response.
    #[test]
    fn test_reverse_requests() {
        // The peer answers an unlock after the host answers its relay request
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(
            &clock,
            Box::new(|session, _, responder| {
                let deadline = session.clock().now() + TIMEOUT;
                // Unlock cannot complete until the application returns the opaque
                // companion response through this reverse request
                let approval = session
                    .requester()
                    .request(
                        RelayArkToAppRequest {
                            id: 42,
                            req: vec![1, 2, 3],
                        },
                        deadline,
                    )
                    .unwrap()
                    .wait::<RelayAppToArkResponse>()
                    .unwrap();
                assert_eq!(approval.id, 42);
                assert_eq!(approval.res, [4, 5, 6]);
                responder
                    .reply(UnlockResponse::default(), deadline)
                    .unwrap()
                    .wait()
                    .unwrap();
                true
            }),
        );

        // The application answers the relay request while the unlock waits
        let (mut ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        let operation = thread::spawn(move || client.call(ManualUnlock, deadline));
        let (request, responder) = ark.recv().unwrap();
        let schema::ark_to_host::Content::RelayReq(request) = request else {
            panic!("expected relay request")
        };
        assert_eq!(request.req, [1, 2, 3]);
        responder
            .reply(
                RelayAppToArkResponse {
                    id: request.id,
                    res: vec![4, 5, 6],
                },
                clock.now() + TIMEOUT,
            )
            .unwrap()
            .wait()
            .unwrap();
        operation.join().unwrap().unwrap();
    }

    /// Request handles can move between threads before choosing where to
    /// decode, even for a response type that is not `Send`.
    #[test]
    fn test_thread_capabilities() {
        // Request completion handles remain Send even when a user's response
        // wrapper isn't Send. The conversion runs in the caller that waits.
        /// Fails compilation if a handle loses its cross-thread guarantee.
        fn assert_send<T: Send>() {}
        assert_send::<Pending<std::rc::Rc<()>>>();
        assert_send::<Client>();
        assert_send::<Ark>();
    }
}
