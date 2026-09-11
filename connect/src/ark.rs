// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Session ownership and typed request handles.

use crate::cloud::Services;
use crate::{Error, Identity, Registration, Request};
use darkbio_wire::protocol::{self, Message, Promise, Requester, Responder, Session, schema};
use darkbio_wire::transport::{self, Verifier};
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Owner of a connection to an Ark. Closing or dropping it ends the session,
/// including requests issued through its [`Client`] handles.
///
/// Receive incoming requests through [`Self::recv`] while other threads issue
/// calls through [`Self::client`]. Wire keeps incoming requests in its bounded
/// queue until received. The application owns dispatch and handler lifetimes.
pub struct Ark {
    /// Wire session whose lifetime owns the connection and its I/O workers.
    session: Session,
    /// Lazy prerequisites shared by all request handles of this session.
    pub(crate) services: Arc<Services>,
}

impl Ark {
    /// Takes ownership of a stream and authenticates the peer under wire's
    /// handshake timeout. Returns the verifier's identity information. Failure
    /// closes the stream.
    pub(crate) fn attach<R, W, V>(
        stream: transport::Stream<R, W>,
        verifier: &V,
    ) -> Result<(Self, V::Info), Error>
    where
        R: transport::Read + Send + 'static,
        W: transport::Write + Send + 'static,
        V: Verifier<Info = Identity>,
    {
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
        let services = Arc::new(Services::new(&info));
        Ok((Self { session, services }, info))
    }

    /// Returns a clonable request handle bound to this session. The handle
    /// does not keep the connection open.
    pub fn client(&self) -> Client {
        Client {
            requester: self.session.requester(),
            services: self.services.clone(),
        }
    }

    /// Blocks for the next known Ark request, or returns the session's ending reason.
    /// Wire answers unknown request types before application dispatch. The returned
    /// responder retains wire's reply completion and automatic reply semantics.
    pub fn recv(&mut self) -> Result<(schema::ark_to_host::Content, Responder), Error> {
        let (message, responder) = self.session.recv()?;
        Ok((message.try_into()?, responder))
    }

    /// Returns a handle for closing the session from another thread, including
    /// while its owner is blocked in [`Self::recv`].
    pub fn closer(&self) -> Closer {
        Closer {
            wire: self.session.closer(),
            services: self.services.clone(),
        }
    }

    /// Closes the session, wakes blocked receives and fails pending requests.
    /// Does not join application handlers or wait for the peer to observe closure.
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

/// Clonable handle for closing the connection and waking callers waiting for setup.
/// Holding the handle does not keep the Ark session open.
#[derive(Clone, Debug)]
pub struct Closer {
    wire: protocol::Closer,  // Closes the original wire session
    services: Arc<Services>, // Ends prerequisite waits on that session
}

impl Closer {
    /// Closes the original connection. An HTTP request already in progress
    /// remains bounded by its deadline; setup waiters are released immediately.
    pub fn close(&self) {
        self.services.close();
        self.wire.close();
    }
}

/// Clonable handle for issuing typed requests through its original session.
/// Each request carries its own deadline. Handles do not keep the session open.
#[derive(Clone, Debug)]
pub struct Client {
    requester: Requester,    // Wire handle bound to the original session
    services: Arc<Services>, // Prerequisite state shared with the owner and other clients
}

impl Client {
    /// Sends a request and waits for its typed response under the supplied deadline.
    /// The deadline covers prerequisite setup, queueing, sending and accepting
    /// the response; decoding is outside it. Reuse it to bound several calls.
    /// Expiration does not cancel an operation the Ark has already received.
    pub fn call<R: Request>(&self, request: R, deadline: Instant) -> Result<R::Response, Error> {
        self.send(request, deadline)?.wait()
    }

    /// Sends a request and waits with a budget starting now. Use [`Self::call`]
    /// with one deadline when several requests must share a budget.
    pub fn call_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<R::Response, Error> {
        self.send_timeout(request, timeout)?.wait()
    }

    /// Establishes the request's prerequisites, then queues it without waiting for
    /// output or a response. The first cloud-dependent send may wait for sync.
    /// Waiting on the promise does not refresh the deadline; dropping
    /// it does not cancel the request. Wire's output queue has no capacity limit,
    /// so the caller bounds the number of outstanding requests.
    pub fn send<R: Request>(
        &self,
        request: R,
        deadline: Instant,
    ) -> Result<Pending<R::Response>, Error> {
        if R::CLOUD_SYNC {
            self.services.sync(&self.requester, deadline)?;
        }
        let promise = self.requester.request(request, deadline)?;
        Ok(Pending {
            promise,
            response: PhantomData,
        })
    }

    /// Establishes prerequisites and queues a request with a budget starting now.
    /// Waiting on the returned promise retains that deadline, even if done later.
    pub fn send_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<Pending<R::Response>, Error> {
        let deadline = Instant::now().checked_add(timeout).ok_or(Error::Timeout)?;
        self.send(request, deadline)
    }

    /// Checks the cloud registry for this attested Ark, synchronizing first if
    /// necessary. Setup, proof generation and HTTP share the supplied deadline.
    /// The returned registration may be inactive; its flags explain why.
    pub fn genuine(&self, deadline: Instant) -> Result<Registration, Error> {
        self.services.genuine(&self.requester, deadline)
    }
}

/// Result of a typed request, decoded when [`Self::wait`] takes the response.
/// Dropping it discards the result without cancelling the request.
#[derive(Debug)]
pub struct Pending<T> {
    /// Encoded response and the notification registered for its completion.
    promise: Promise<Message>,
    /// Response type selected by the request, without owning a value of it.
    response: PhantomData<fn() -> T>,
}

impl<T> Pending<T> {
    /// Sends an event when the request completes, successfully or with an error.
    /// One channel can observe many requests. Registration leaves the response
    /// encoded until [`Self::wait`] and does not change its deadline.
    ///
    /// # Panics
    ///
    /// Panics if a notification was already registered on this promise.
    pub fn notify<E: Copy + Send + 'static>(&mut self, sender: mpsc::Sender<E>, event: E) {
        self.promise.notify(sender, event);
    }
}

impl<T: TryFrom<Message, Error = protocol::Error>> Pending<T> {
    /// Waits for completion and decodes the expected response. An accepted
    /// response remains available after its deadline or the session's closure.
    pub fn wait(self) -> Result<T, Error> {
        Ok(self.promise.wait()?)
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
    use crate::testing::{Peer, answering, hangup, silent};
    use std::thread;

    /// Budget for test I/O that is not exercising expiration.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Application error codes reach the peer, and a reserved refusal leaves the
    /// session available for subsequent requests.
    #[test]
    fn test_coded_errors() {
        /// Application refusal returned by the host's companion handler.
        #[derive(Debug, thiserror::Error)]
        #[error("companion rejected authorization")]
        struct Denied;

        impl crate::CodedError for Denied {
            fn code(&self) -> u64 {
                0x100
            }
        }

        let mut peer = Peer::spawn(Box::new(|session, request, responder| {
            if !matches!(request, schema::host_to_ark::Content::Unlock(_)) {
                return answering(session, request, responder);
            }
            let deadline = Instant::now() + TIMEOUT;
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
        }));
        let (mut ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client
            .send(UnlockRequest {}, Instant::now() + TIMEOUT)
            .unwrap();
        let (request, responder) = ark.recv().unwrap();
        assert!(matches!(request, schema::ark_to_host::Content::RelayReq(_)));
        responder
            .fail(Denied, Instant::now() + TIMEOUT)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            pending.wait(), Err(Error::Remote(error))
                if error.code == schema::ReservedErrors::Unavailable as u64
        ));
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
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
        use darkbio_wire::{memory, transport};
        use prost::Message as _;

        /// Future Ark envelope carrying content absent from the current wire schema.
        #[derive(prost::Message)]
        struct FutureRequest {
            #[prost(uint64, tag = "1")]
            id: u64,
            #[prost(bytes = "vec", tag = "2047")]
            content: Vec<u8>,
        }

        let signer = xdsa::SecretKey::generate();
        let identity = signer.public_key();
        let attestation = self_attestation(&signer, identity.clone());
        let (host, remote) = memory::duplex(256 * 1024);
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
        let (mut ark, _) =
            Ark::attach(host, &crate::TrustMode::Recover(Box::new(identity))).unwrap();
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
                Instant::now() + TIMEOUT,
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

    /// Concurrent typed requests retain their responses and completion tokens.
    /// A reserved peer refusal is returned through the same request interface.
    #[test]
    fn test_requests() {
        let mut peer = Peer::spawn(Box::new(answering));
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let (completed, events) = mpsc::channel();
        let mut pending = client
            .send(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
            .unwrap();
        pending.notify(completed, 7);
        assert_eq!(events.recv_timeout(TIMEOUT).unwrap(), 7);
        assert_eq!(pending.wait().unwrap().firmware_version, "1.0.0");

        let callers: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                thread::spawn(move || {
                    client
                        .call(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
                        .unwrap()
                        .firmware_version
                })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), "1.0.0");
        }
        assert!(
            matches!(client.call(OnboardingRequest::default(), Instant::now() + TIMEOUT), Err(Error::Remote(error)) if error.code == schema::ReservedErrors::Unsupported as u64)
        );
    }

    /// Dropping the owner closes pending requests and refuses surviving clients.
    #[test]
    fn test_owner_drop() {
        let mut peer = Peer::spawn(silent());
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client
            .send(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
            .unwrap();
        drop(ark);
        assert!(matches!(pending.wait(), Err(Error::Closed)));
        assert!(matches!(
            client.call(DeviceInfoRequest {}, Instant::now() + TIMEOUT),
            Err(Error::Closed)
        ));
    }

    /// A closer wakes both the receive loop and outstanding requests.
    #[test]
    fn test_close() {
        let mut peer = Peer::spawn(silent());
        let (mut ark, _) = peer.attach().unwrap();
        let pending = ark
            .client()
            .send(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
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
        let mut peer = Peer::spawn(hangup());
        let (mut ark, _) = peer.attach().unwrap();
        assert!(matches!(
            ark.client()
                .call(DeviceInfoRequest {}, Instant::now() + TIMEOUT),
            Err(Error::Disconnected(_))
        ));
        assert!(matches!(ark.recv(), Err(Error::Disconnected(_))));
        assert!(matches!(
            ark.client()
                .call(DeviceInfoRequest {}, Instant::now() + TIMEOUT),
            Err(Error::Disconnected(_))
        ));
    }

    /// A short request timeout leaves a concurrent request's budget intact.
    #[test]
    fn test_timeouts() {
        let mut peer = Peer::spawn(silent());
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let pending = client.send_timeout(DeviceInfoRequest {}, TIMEOUT).unwrap();
        assert!(matches!(
            client
                .clone()
                .call_timeout(DeviceInfoRequest {}, Duration::from_millis(20)),
            Err(Error::Timeout)
        ));
        ark.close();
        assert!(matches!(pending.wait(), Err(Error::Closed)));
    }

    /// Reusing a deadline across calls and cloned handles does not renew its budget.
    #[test]
    fn test_deadlines() {
        let mut peer = Peer::spawn(Box::new(answering));
        let (ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, deadline)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
        // Spend the remaining operation budget before issuing the next request.
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        assert!(matches!(
            client.clone().call(DeviceInfoRequest {}, deadline),
            Err(Error::Timeout)
        ));
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
        let mut peer = Peer::spawn(Box::new(|session, _, responder| {
            let deadline = Instant::now() + TIMEOUT;
            // Unlock cannot complete until the application returns the opaque
            // companion response through this reverse request.
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
        }));
        let (mut ark, _) = peer.attach().unwrap();
        let client = ark.client();
        let operation =
            thread::spawn(move || client.call(UnlockRequest {}, Instant::now() + TIMEOUT));
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
                Instant::now() + TIMEOUT,
            )
            .unwrap()
            .wait()
            .unwrap();
        operation.join().unwrap().unwrap();
    }

    /// Request handles can move between threads before selecting where to decode
    /// a response, including a response type that cannot itself move between threads.
    #[test]
    fn test_thread_capabilities() {
        // Request completion handles remain Send even when a user's response wrapper
        // isn't Send. The conversion runs in the caller that waits.
        fn assert_send<T: Send>() {}
        assert_send::<Pending<std::rc::Rc<()>>>();
        assert_send::<Client>();
        assert_send::<Ark>();
    }
}
