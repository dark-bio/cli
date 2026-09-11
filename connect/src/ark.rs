// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! A connected Ark as the caller sees it, the requests of the protobuf
//! protocol as typed calls over a wire session. A thread of its own receives
//! the requests the Ark sends and hands them to the handler along with their
//! responders, the callers sending through the session's requester.

use crate::{Error, Request};
use darkbio_wire::protocol::schema::*;
use darkbio_wire::protocol::{self, Closer, Message, Promise, Requester, Session, schema};
use darkbio_wire::transport::{self, Verifier};
use std::io;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, warn};

/// How long a request waits on the Ark unless told otherwise, the budget of
/// every request and reply without a deadline of its own. The handshake has
/// the wire's own budget.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Operational domain a connected device belongs to, live for genuine Arks
/// and sandbox for emulated ones. It selects the trust roots the device's
/// attestation is validated against and the cloud API tree it is served from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Realm {
    Live,
    Sandbox,
}

/// Maps a failed handshake to the connection's error, a read running into the
/// deadline being the Ark timing out rather than a failure of the wire.
fn handshake_error(err: protocol::Error) -> Error {
    if let protocol::Error::Transport(err) = &err
        && let transport::Error::RecvFailed(io) = &**err
        && matches!(
            io.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        )
    {
        return Error::Timeout;
    }
    Error::Handshake(err)
}

/// Handler of the requests the Ark sends on its own, run on the session's
/// serving thread with the responder to answer through.
type RequestHandler = Box<dyn FnMut(ark_to_host::Content, Responder) + Send>;

/// Handler of the session ending without a close, run once, on the serving
/// thread that saw it end or right away when registered after that.
type DisconnectHandler = Box<dyn FnOnce(protocol::Error) + Send>;

/// The session's end as the disconnect handler learns of it.
enum Disconnect {
    Pending(Option<DisconnectHandler>), // Session alive, with the handler to tell if one was registered
    Closed,                             // Session closed locally, nothing to tell
    Ended(protocol::Error), // Session ended without a close, the reason told to every handler
}

/// State shared between the Ark and its serving thread.
struct Shared {
    session: Mutex<Option<Session>>, // Parked once serving ends, so later requests still name the reason
    handler: Mutex<Option<RequestHandler>>, // Handler of the Ark's requests, taken out while it runs
    disconnect: Mutex<Disconnect>, // How the session ended, or the handler to tell when it does
    timeout: Mutex<Duration>,      // Deadline of requests and replies without one of their own
}

/// A connected Ark, its requests issued from any thread and answered on their
/// own, the requests it sends on its own handed to a handler and the session
/// ending reported. Dropping it closes the session.
pub struct Ark {
    realm: Realm,         // Domain the Ark belongs to
    requester: Requester, // Sends requests through the session
    closer: Closer,       // Ends the session from any thread
    shared: Arc<Shared>,  // State shared with the serving thread
}

impl Ark {
    /// Runs the wire handshake over a stream of the realm and wraps the
    /// session, the verifier deciding whether to trust the attestation the
    /// Ark presents. The handshake waits on the Ark under the wire's own
    /// budget and failure closes the stream. The requests of the session
    /// wait `DEFAULT_TIMEOUT` unless changed on it.
    pub fn attach<R, W, V>(
        stream: transport::Stream<R, W>,
        realm: Realm,
        verifier: &V,
    ) -> Result<(Ark, V::Info), Error>
    where
        R: transport::Read + Send + 'static,
        W: transport::Write + Send + 'static,
        V: Verifier,
    {
        let (session, info) = protocol::connect(stream, verifier).map_err(handshake_error)?;
        Ok((Ark::new(realm, session), info))
    }

    /// Wraps an established session, its serving thread handing out the
    /// requests the Ark sends until the session ends.
    fn new(realm: Realm, session: Session) -> Self {
        let shared = Arc::new(Shared {
            session: Mutex::new(None),
            handler: Mutex::new(None),
            disconnect: Mutex::new(Disconnect::Pending(None)),
            timeout: Mutex::new(DEFAULT_TIMEOUT),
        });
        let ark = Self {
            realm,
            requester: session.requester(),
            closer: session.closer(),
            shared: shared.clone(),
        };
        thread::Builder::new()
            .name("ark-serve".into())
            .spawn(move || serve(shared, session))
            .expect("failed to spawn the serving thread");
        ark
    }

    /// Domain the Ark belongs to.
    pub fn realm(&self) -> Realm {
        self.realm
    }

    /// Deadline of the requests and replies without one of their own.
    pub fn timeout(&self) -> Duration {
        *self.shared.timeout.lock().unwrap()
    }

    /// Sets the deadline of the requests and replies without one of their own.
    pub fn set_timeout(&self, timeout: Duration) {
        *self.shared.timeout.lock().unwrap() = timeout;
    }

    /// Registers the handler of the requests the Ark sends on its own,
    /// replacing the previous one, from inside a handler too. Each comes
    /// with the responder to answer it through, a responder dropped
    /// unanswered replying that on its own. The handler runs on the serving
    /// thread, so the requests the Ark sends after it wait for it, the
    /// answers to the host's own requests do not. Hand the request to
    /// another thread to work on it at length.
    pub fn on_request(
        &self,
        handler: impl FnMut(ark_to_host::Content, Responder) + Send + 'static,
    ) {
        *self.shared.handler.lock().unwrap() = Some(Box::new(handler));
    }

    /// Registers the handler invoked when the session ends without a close,
    /// the reason telling a USB unplug or the emulator shutting down apart
    /// from the Ark dropping the session. A session that already ended tells
    /// the handler right away.
    pub fn on_disconnect(&self, handler: impl FnOnce(protocol::Error) + Send + 'static) {
        let reason = {
            let mut disconnect = self.shared.disconnect.lock().unwrap();
            match &mut *disconnect {
                Disconnect::Pending(slot) => {
                    *slot = Some(Box::new(handler));
                    return;
                }
                Disconnect::Closed => return,
                Disconnect::Ended(reason) => reason.clone(),
            }
        };
        handler(reason);
    }

    /// Closes the session and the transport underneath, failing the requests
    /// in flight. Dropping the Ark does the same.
    pub fn close(&self) {
        self.closer.close();
    }

    /// Sends a request and waits for its answer within the default timeout,
    /// an error the Ark answered with surfacing as a remote error.
    pub fn call<R: Request>(&self, request: R) -> Result<R::Response, Error> {
        self.call_timeout(request, self.timeout())
    }

    /// Sends a request and waits for its answer within the timeout, which
    /// covers the queue, the send and the wait. An error the Ark answered
    /// with surfaces as a remote error.
    pub fn call_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<R::Response, Error> {
        self.send_timeout(request, timeout)?.wait()
    }

    /// Sends a request without waiting for its answer, which waiting on the
    /// pending result yields within the default timeout. Sending the next
    /// request before waiting keeps the wire busy.
    pub fn send<R: Request>(&self, request: R) -> Result<Pending<R::Response>, Error> {
        self.send_timeout(request, self.timeout())
    }

    /// Sends a request without waiting for its answer, which waiting on the
    /// pending result yields within the timeout. The timeout covers the
    /// queue, the send and the wait, whenever the caller gets to it.
    pub fn send_timeout<R: Request>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<Pending<R::Response>, Error> {
        let deadline = Instant::now() + timeout;
        let promise = self.requester.request(request, deadline)?;
        Ok(Pending {
            promise,
            response: PhantomData,
        })
    }

    /// Retrieves the hardware and firmware version info of the Ark.
    pub fn device_info(&self) -> Result<DeviceInfoResponse, Error> {
        self.call(DeviceInfoRequest {})
    }

    /// Injects a root-signed device attestation into the Ark, which verifies
    /// it against its own trust roots and persists it as its identity.
    pub fn onboard(&self, attestation: Vec<u8>) -> Result<OnboardingResponse, Error> {
        self.call(OnboardingRequest {
            device_attestation: attestation,
        })
    }

    /// Requests a proof for device authenticity checks.
    pub fn genuinity_proof(&self) -> Result<GenuinityProofResponse, Error> {
        self.call(GenuinityProofRequest {})
    }

    /// Unlocks the Ark, waiting for the approval within the timeout.
    pub fn unlock(&self, timeout: Duration) -> Result<UnlockResponse, Error> {
        self.call_timeout(UnlockRequest {}, timeout)
    }

    /// Starts a cloud synchronization with the attestations of the cloud's
    /// signing and encryption keys.
    pub fn cloud_sync_start(
        &self,
        signer: Vec<u8>,
        crypto: Vec<u8>,
    ) -> Result<CloudSyncStartResponse, Error> {
        self.call(CloudSyncStartRequest { signer, crypto })
    }

    /// Finishes a cloud synchronization with the cloud's signed timestamp.
    pub fn cloud_sync_finish(
        &self,
        unixmilli: u64,
        signature: Vec<u8>,
    ) -> Result<CloudSyncFinishResponse, Error> {
        self.call(CloudSyncFinishRequest {
            unixmilli,
            signature,
        })
    }

    /// Prepares a firmware update to the version, the hash and the size being
    /// those of the encrypted archive.
    pub fn firmware_update_prep(
        &self,
        version: &str,
        sha256: Vec<u8>,
        bytes: u64,
    ) -> Result<FirmwareUpdatePrepResponse, Error> {
        self.call(FirmwareUpdatePrepRequest {
            version: version.to_owned(),
            sha256,
            bytes,
        })
    }

    /// Initializes a firmware update with the sealed access to its archive.
    pub fn firmware_update_init(
        &self,
        access: Vec<u8>,
    ) -> Result<FirmwareUpdateInitResponse, Error> {
        self.call(FirmwareUpdateInitRequest { access })
    }

    /// Uploads a chunk of the firmware archive.
    pub fn firmware_update_upload(
        &self,
        chunk: Vec<u8>,
    ) -> Result<FirmwareUpdateUploadResponse, Error> {
        self.call(FirmwareUpdateUploadRequest { chunk })
    }

    /// Verifies an uploaded firmware update.
    pub fn firmware_update_verify(&self) -> Result<FirmwareUpdateVerifyResponse, Error> {
        self.call(FirmwareUpdateVerifyRequest {})
    }

    /// Installs a verified firmware update.
    pub fn firmware_update_install(&self) -> Result<FirmwareUpdateInstallResponse, Error> {
        self.call(FirmwareUpdateInstallRequest {})
    }

    /// Requests the current pairing status.
    pub fn pairing_status(&self) -> Result<PairingStatusResponse, Error> {
        self.call(PairingStatusRequest {})
    }

    /// Initiates a pairing.
    pub fn pairing_auth(&self) -> Result<PairingAuthResponse, Error> {
        self.call(PairingAuthRequest {})
    }

    /// Injects the companion app's sealed identity.
    pub fn pairing_set_app_id(
        &self,
        identity: Vec<u8>,
    ) -> Result<PairingSetAppIdentityResponse, Error> {
        self.call(PairingSetAppIdentityRequest { identity })
    }

    /// Injects the companion app's sealed storage key material.
    pub fn pairing_set_app_storage(
        &self,
        app_key: Vec<u8>,
    ) -> Result<PairingSetAppStorageResponse, Error> {
        self.call(PairingSetAppStorageRequest { app_key })
    }

    /// Confirms with the app's sealed acknowledgement that the Ark's key
    /// material was received.
    pub fn pairing_ack_ark_storage(
        &self,
        app_ack: Vec<u8>,
    ) -> Result<PairingAckArkStorageResponse, Error> {
        self.call(PairingAckArkStorageRequest { app_ack })
    }

    /// Waits for the pairing to be accepted, within the timeout.
    pub fn pairing_accept(&self, timeout: Duration) -> Result<PairingAcceptanceResponse, Error> {
        self.call_timeout(PairingAcceptanceRequest {}, timeout)
    }

    /// Completes the pairing.
    pub fn pairing_complete(&self) -> Result<PairingCompletionResponse, Error> {
        self.call(PairingCompletionRequest {})
    }

    /// Requests permission to join the relay.
    pub fn relay_join(&self) -> Result<RelayJoinResponse, Error> {
        self.call(RelayJoinRequest {})
    }

    /// Forwards a sealed request of the companion app to the Ark, under the
    /// app's request id.
    pub fn relay_req(&self, id: u64, req: Vec<u8>) -> Result<RelayArkToAppResponse, Error> {
        self.call(RelayAppToArkRequest { id, req })
    }

    /// Starts uploading an executable of the size.
    pub fn exec_upload_start(&self, bytes: u64) -> Result<ExecutionUploadStartResponse, Error> {
        self.call(ExecutionUploadStartRequest { bytes })
    }

    /// Uploads a chunk of the executable of the task.
    pub fn exec_upload_chunk(
        &self,
        taskid: u64,
        chunk: Vec<u8>,
    ) -> Result<ExecutionUploadChunkResponse, Error> {
        self.call(ExecutionUploadChunkRequest { taskid, chunk })
    }

    /// Schedules the uploaded executable of the task.
    pub fn exec_sched(&self, taskid: u64) -> Result<ExecutionScheduleResponse, Error> {
        self.call(ExecutionScheduleRequest { taskid })
    }

    /// Requests the status of the task's execution.
    pub fn exec_status(&self, taskid: u64) -> Result<ExecutionStatusResponse, Error> {
        self.call(ExecutionStatusRequest { taskid })
    }

    /// Cancels the task's execution.
    pub fn exec_cancel(&self, taskid: u64) -> Result<ExecutionCancelResponse, Error> {
        self.call(ExecutionCancelRequest { taskid })
    }

    /// Lists the dataset slots.
    pub fn slot_list(&self) -> Result<SlotListResponse, Error> {
        self.call(SlotListRequest {})
    }

    /// Repairs a dataset slot, resetting it to empty.
    pub fn slot_repair(&self, slot: SlotKind) -> Result<SlotRepairResponse, Error> {
        self.call(SlotRepairRequest { slot: slot as i32 })
    }

    /// Deletes a dataset slot.
    pub fn slot_delete(&self, slot: SlotKind) -> Result<SlotDeleteResponse, Error> {
        self.call(SlotDeleteRequest { slot: slot as i32 })
    }

    /// Identifies a dataset from its file name, size and first chunk, among
    /// the slot kinds given or any if none.
    pub fn slot_upload_peek(
        &self,
        name: &str,
        size: u64,
        chunk: Vec<u8>,
        kinds: &[SlotKind],
    ) -> Result<SlotUploadPeekResponse, Error> {
        self.call(SlotUploadPeekRequest {
            name: name.to_owned(),
            size,
            chunk,
            kinds: kinds.iter().map(|kind| *kind as i32).collect(),
        })
    }

    /// Starts uploading a dataset into a slot, its file name, size and first
    /// chunk checked against the kind.
    pub fn slot_upload_start(
        &self,
        kind: SlotKind,
        name: &str,
        size: u64,
        chunk: Vec<u8>,
    ) -> Result<SlotUploadStartResponse, Error> {
        self.call(SlotUploadStartRequest {
            kind: kind as i32,
            name: name.to_owned(),
            size,
            chunk,
        })
    }

    /// Uploads a chunk of the dataset of the upload session.
    pub fn slot_upload_chunk(
        &self,
        session: u64,
        chunk: Vec<u8>,
    ) -> Result<SlotUploadChunkResponse, Error> {
        self.call(SlotUploadChunkRequest { session, chunk })
    }

    /// Cancels the dataset upload session.
    pub fn slot_upload_cancel(&self, session: u64) -> Result<SlotUploadCancelResponse, Error> {
        self.call(SlotUploadCancelRequest { session })
    }

    /// Processes the uploaded dataset of the session into its slot.
    pub fn slot_upload_process(&self, session: u64) -> Result<SlotUploadProcessResponse, Error> {
        self.call(SlotUploadProcessRequest { session })
    }
}

impl Drop for Ark {
    fn drop(&mut self) {
        self.close();
    }
}

/// A request sent and not yet answered, the answer taken by waiting on it.
/// Dropping it discards the answer, not the request, which the Ark may keep
/// working on.
#[derive(Debug)]
pub struct Pending<T> {
    promise: Promise<Message>, // Answer of the wire, still encoded
    response: PhantomData<T>,  // Body the answer is expected to be
}

impl<T> Pending<T>
where
    T: TryFrom<Message, Error = protocol::Error>,
{
    /// Waits for the answer within the deadline the request was sent with,
    /// an error the Ark answered with surfacing as a remote error.
    pub fn wait(self) -> Result<T, Error> {
        Ok(self.promise.wait()?)
    }
}

/// Answers one request the Ark sent, within the Ark's timeout. Dropped
/// unanswered, the wire tells the Ark so on its own.
#[derive(Debug)]
pub struct Responder {
    inner: protocol::Responder, // Responder of the wire, answering once
    timeout: Duration,          // Deadline of the reply, the Ark's when the request arrived
}

impl Responder {
    /// Answers the request with a response body, queued for sending. A closed
    /// session refuses it.
    pub fn reply(self, response: impl Into<Message>) -> Result<(), Error> {
        self.inner.reply(response, Instant::now() + self.timeout)?;
        Ok(())
    }

    /// Answers the request with an error of the application, queued for
    /// sending. A closed session refuses it.
    pub fn fail(self, error: schema::Error) -> Result<(), Error> {
        self.inner.fail(error, Instant::now() + self.timeout)?;
        Ok(())
    }
}

/// Receives the requests the Ark sends until the session ends, handing each to
/// the handler with a responder carrying the timeout of the moment. The handler
/// runs outside its slot, so it may replace itself, the replacement taking the
/// slot it left empty. A body the Ark cannot send is dropped, as is a request
/// without a handler, the responder answering unanswered on the way out. A
/// handler panicking is its own bug, the session carries on. The session is
/// parked afterwards, so later requests are refused with the reason it ended.
/// A close needs no notification, anything else tells the disconnect handler,
/// right away if one is registered, else once it is.
fn serve(shared: Arc<Shared>, mut session: Session) {
    let reason = loop {
        let (message, responder) = match session.recv() {
            Ok(received) => received,
            Err(err) => break err,
        };
        let responder = Responder {
            inner: responder,
            timeout: *shared.timeout.lock().unwrap(),
        };
        let request = match ark_to_host::Content::try_from(message) {
            Ok(request) => request,
            Err(err) => {
                warn!("dropping request the ark cannot send: {}", err);
                continue;
            }
        };
        let handler = shared.handler.lock().unwrap().take();
        match handler {
            Some(mut handler) => {
                if panic::catch_unwind(AssertUnwindSafe(|| handler(request, responder))).is_err() {
                    error!("request handler panicked");
                }
                let mut slot = shared.handler.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(handler);
                }
            }
            None => warn!("dropping request without a handler"),
        }
    };
    *shared.session.lock().unwrap() = Some(session);

    // Record how the session ended and take the handler waiting for that,
    // told outside the lock so it may register another
    let reason = match reason {
        protocol::Error::Closed => None,
        reason => Some(reason),
    };
    let handler = {
        let mut disconnect = shared.disconnect.lock().unwrap();
        let ended = match &reason {
            None => Disconnect::Closed,
            Some(reason) => Disconnect::Ended(reason.clone()),
        };
        match std::mem::replace(&mut *disconnect, ended) {
            Disconnect::Pending(handler) => handler,
            _ => None,
        }
    };
    if let (Some(reason), Some(handler)) = (reason, handler) {
        handler(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Peer, answering, hangup, silent};
    use std::sync::mpsc;

    // Tests that a typed request goes through the session and its response
    // comes back typed, an error the Ark answered with surfacing as a remote
    // error.
    #[test]
    fn test_request() {
        let mut peer = Peer::spawn(Box::new(answering));
        let (ark, _) = peer.attach().unwrap();
        assert_eq!(ark.realm(), Realm::Sandbox);

        let info = ark.device_info().unwrap();
        assert_eq!(info.firmware_version, "1.0.0");

        let err = ark.slot_list().unwrap_err();
        assert!(
            matches!(err, Error::Remote(schema::Error { code: 7, .. })),
            "{err:?}"
        );
        ark.close();
    }

    // Tests that requests sent ahead of their answers are each answered to
    // their own pending result.
    #[test]
    fn test_pipelining() {
        let mut peer = Peer::spawn(Box::new(answering));
        let (ark, _) = peer.attach().unwrap();

        let pending: Vec<_> = (0..4)
            .map(|_| ark.send(DeviceInfoRequest {}).unwrap())
            .collect();
        for pending in pending {
            assert_eq!(pending.wait().unwrap().firmware_version, "1.0.0");
        }
    }

    // Tests that requests from several threads are answered each to its own
    // caller, the responses matched by id.
    #[test]
    fn test_concurrent_requests() {
        let mut peer = Peer::spawn(Box::new(answering));
        let ark = Arc::new(peer.attach().unwrap().0);

        let callers: Vec<_> = (0..8)
            .map(|_| {
                let ark = ark.clone();
                thread::spawn(move || ark.device_info().unwrap().firmware_version)
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), "1.0.0");
        }
    }

    // Tests that the requests the Ark sends on its own reach the handler, and
    // that a handler panicking neither ends the session nor the serving
    // thread.
    #[test]
    fn test_events() {
        let mut peer = Peer::spawn(Box::new(answering));
        let (ark, _) = peer.attach().unwrap();

        let (tx, rx) = mpsc::channel();
        ark.on_request(move |request, _| tx.send(request).unwrap());
        ark.unlock(DEFAULT_TIMEOUT).unwrap();
        assert!(matches!(
            rx.recv_timeout(DEFAULT_TIMEOUT).unwrap(),
            ark_to_host::Content::DeviceInfo(_)
        ));

        ark.on_request(|_, _| panic!("handler boom"));
        ark.unlock(DEFAULT_TIMEOUT).unwrap();
        assert_eq!(ark.device_info().unwrap().firmware_version, "1.0.0");
    }

    // Tests that a handler may replace itself from inside its own callback,
    // the replacement taking the requests that follow.
    #[test]
    fn test_handler_replacement() {
        let mut peer = Peer::spawn(Box::new(answering));
        let ark = Arc::new(peer.attach().unwrap().0);

        let (tx, rx) = mpsc::channel();
        let weak = Arc::downgrade(&ark);
        ark.on_request({
            let tx = tx.clone();
            move |_, _| {
                if let Some(ark) = weak.upgrade() {
                    let tx = tx.clone();
                    ark.on_request(move |_, _| tx.send(2).unwrap());
                }
                tx.send(1).unwrap();
            }
        });
        ark.unlock(DEFAULT_TIMEOUT).unwrap();
        ark.unlock(DEFAULT_TIMEOUT).unwrap();
        assert_eq!(rx.recv_timeout(DEFAULT_TIMEOUT).unwrap(), 1);
        assert_eq!(rx.recv_timeout(DEFAULT_TIMEOUT).unwrap(), 2);
        ark.close();
    }

    // Tests that a request nobody answers times out on its own, the session
    // outliving it.
    #[test]
    fn test_timeouts() {
        let mut peer = Peer::spawn(silent());
        let (ark, _) = peer.attach().unwrap();
        ark.set_timeout(Duration::from_millis(50));
        assert!(matches!(ark.device_info(), Err(Error::Timeout)));
        assert!(matches!(ark.device_info(), Err(Error::Timeout)));
        ark.close();
    }

    // Tests that the Ark going away ends the session, the request in flight
    // and every later one failing with the reason and the disconnect handler
    // told once, a handler registered after the end told right away.
    #[test]
    fn test_disconnect() {
        let mut peer = Peer::spawn(hangup());
        let (ark, _) = peer.attach().unwrap();

        let (tx, rx) = mpsc::channel();
        ark.on_disconnect(move |reason| tx.send(reason).unwrap());
        let err = ark.device_info().unwrap_err();
        assert!(matches!(err, Error::Disconnected(_)), "{err:?}");
        assert!(matches!(
            rx.recv_timeout(DEFAULT_TIMEOUT).unwrap(),
            protocol::Error::Transport(_)
        ));
        assert!(matches!(ark.device_info(), Err(Error::Disconnected(_))));

        let (tx, rx) = mpsc::channel();
        ark.on_disconnect(move |reason| tx.send(reason).unwrap());
        assert!(matches!(
            rx.try_recv().unwrap(),
            protocol::Error::Transport(_)
        ));
    }

    // Tests that closing the Ark fails the requests in flight and refuses new
    // ones, without a disconnect notification, before or after the close.
    #[test]
    fn test_close() {
        let mut peer = Peer::spawn(silent());
        let ark = Arc::new(peer.attach().unwrap().0);

        let (tx, rx) = mpsc::channel();
        ark.on_disconnect(move |reason| tx.send(reason).unwrap());
        let pending = {
            let ark = ark.clone();
            thread::spawn(move || ark.device_info())
        };
        thread::sleep(Duration::from_millis(50));
        ark.close();
        assert!(matches!(pending.join().unwrap(), Err(Error::Closed)));
        assert!(matches!(ark.device_info(), Err(Error::Closed)));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

        let (tx, rx) = mpsc::channel();
        ark.on_disconnect(move |reason| tx.send(reason).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
}
