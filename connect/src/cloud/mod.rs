// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Cloud prerequisites and registry checks for an Ark connection.

mod dns;
mod firmware;
mod http;
mod relay;

pub use firmware::{Firmware, UpdateProgress};
pub(crate) use http::PackageAuth;
pub use http::Registration;

use crate::schema::{
    GenuinityProofRequest, RelayArkToAppRequest, RelayJoinRequest, RelayJoinResponse,
};
use crate::{Error, Identity};
use darkbio_wire::protocol::{self, Requester, Responder};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Setup state shared by every client of one connection. Network and device I/O
/// run outside its lock so independent requests and closure remain available.
#[derive(Debug)]
pub(crate) struct Services {
    cloud: Option<http::Api>, // Absent without an attested or caller-supplied environment
    state: Mutex<State>,      // Current initialization attempt and connection lifecycle
    updating: Mutex<()>,      // One firmware transfer at a time across client clones
}

/// Initialization progresses once at a time and becomes reusable only on success.
#[derive(Debug, Default)]
struct State {
    synced: bool,                   // Whether an automatic sync completed on this connection
    error: Option<protocol::Error>, // Why the owning wire session ended
    syncing: Option<Arc<Attempt>>,  // Cloud sync joined by concurrent callers
    relay: Option<relay::Relay>,    // Relay attached lazily to this connection
    joining: Option<Arc<Attempt>>,  // Relay attachment joined by concurrent callers
}

/// One attempt's outcome, retained by its waiters even after a retry starts.
#[derive(Debug, Default)]
struct Attempt {
    result: Mutex<Option<Result<(), Failure>>>, // Shared success or the original failure
    ready: Condvar,                             // Wakes waiters on completion or closure
}

/// Failures shareable between callers joining the same initialization attempt.
#[derive(Clone, Debug)]
enum Failure {
    Cloud(String),         // HTTP or response decoding failure
    Relay(String),         // Relay connection or envelope failure
    Wire(protocol::Error), // Device failure, retaining remote codes and disconnect reasons
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Self::Cloud(error)
    }
}

impl From<protocol::Error> for Failure {
    fn from(error: protocol::Error) -> Self {
        Self::Wire(error)
    }
}

impl From<Failure> for Error {
    fn from(error: Failure) -> Self {
        match error {
            Failure::Cloud(error) => Self::Cloud(error),
            Failure::Relay(error) => Self::Relay(error),
            Failure::Wire(error) => error.into(),
        }
    }
}

impl Services {
    /// A stopped dispatcher has dropped its wire session. Preserve its ending
    /// reason when a surviving weak requester can only report that it is gone.
    pub(crate) fn wire_error(&self, error: protocol::Error) -> Error {
        if matches!(error, protocol::Error::Closed)
            && let Some(ended) = &self.state.lock().expect("cloud setup not poisoned").error
        {
            return ended.clone().into();
        }
        error.into()
    }

    /// Records cloud routing without starting network I/O.
    pub(crate) fn new(
        identity: &Identity,
        cloud: Option<(crate::trust::Environment, crate::trust::Realm)>,
    ) -> Self {
        Self {
            cloud: http::Api::new(identity, cloud),
            state: Mutex::new(State::default()),
            updating: Mutex::new(()),
        }
    }

    /// Synchronizes once per connection. Each caller bounds its own wait, and
    /// the caller starting the exchange supplies its I/O deadline.
    pub(crate) fn sync(&self, requester: &Requester, deadline: Instant) -> Result<(), Error> {
        self.ensure(requester, Step::Sync, deadline)
    }

    /// Attaches the relay after cloud sync, reusing a healthy connection. A
    /// failed relay is replaced on the next call without replaying any operation.
    pub(crate) fn relay(&self, requester: &Requester, deadline: Instant) -> Result<(), Error> {
        self.sync(requester, deadline)?;
        self.ensure(requester, Step::Relay, deadline)
    }

    /// Serializes one prerequisite while allowing unrelated device traffic.
    fn ensure(&self, requester: &Requester, step: Step, deadline: Instant) -> Result<(), Error> {
        let (attempt, leader) = {
            let mut state = self.state.lock().expect("cloud setup not poisoned");
            if let Some(error) = &state.error {
                return Err(error.clone().into());
            }
            match step {
                _ if self.cloud.is_none() => return Err(Error::MissingEnvironment),
                Step::Sync if state.synced => return Ok(()),
                Step::Relay if state.relay.as_ref().is_some_and(relay::Relay::connected) => {
                    return Ok(());
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let pending = match step {
                Step::Sync => &mut state.syncing,
                Step::Relay => &mut state.joining,
            };
            match pending {
                Some(attempt) => (attempt.clone(), false),
                None => {
                    let attempt = Arc::new(Attempt::default());
                    *pending = Some(attempt.clone());
                    (attempt, true)
                }
            }
        };
        if !leader {
            return attempt.wait(deadline).map_err(Into::into);
        }
        let result = match step {
            Step::Sync => self.synchronize(requester, deadline).map(|()| None),
            Step::Relay => self.join(requester, deadline).map(Some),
        };
        let mut state = self.state.lock().expect("cloud setup not poisoned");
        let result = if let Some(error) = &state.error {
            Err(Failure::Wire(error.clone()))
        } else {
            result.and_then(|relay| {
                match step {
                    Step::Sync => state.synced = true,
                    Step::Relay => {
                        let mut relay = relay.expect("relay setup returned an attachment");
                        relay.start()?;
                        state.relay = Some(relay);
                    }
                }
                Ok(())
            })
        };
        match step {
            Step::Sync => state.syncing = None,
            Step::Relay => state.joining = None,
        }
        attempt.finish(result.clone());
        result.map_err(Into::into)
    }

    /// Authenticates relay attachment with a fresh authorization from the Ark.
    fn join(&self, requester: &Requester, deadline: Instant) -> Result<relay::Relay, Failure> {
        let cloud = self.cloud.as_ref().expect("cloud route available");
        let joined = requester
            .request(RelayJoinRequest {}, deadline)?
            .wait::<RelayJoinResponse>()?;
        relay::Relay::connect(
            &cloud.relay_url(),
            &joined.auth,
            requester.clone(),
            deadline,
        )
    }

    /// Attaches on demand when the Ark conditionally needs authorization. Wire
    /// replies progress independently of this dispatcher waiting for attachment.
    /// Connections without a cloud route retain their explicit receive interface.
    pub(crate) fn forward(
        &self,
        requester: &Requester,
        request: RelayArkToAppRequest,
        responder: Responder,
    ) -> Option<(RelayArkToAppRequest, Responder)> {
        if self.cloud.is_none() {
            return Some((request, responder));
        }
        let deadline = Instant::now() + relay::EXCHANGE_TIMEOUT;
        if let Err(error) = self.relay(requester, deadline) {
            relay::fail(responder, &error.to_string());
            return None;
        }
        let state = self.state.lock().expect("cloud setup not poisoned");
        match &state.relay {
            Some(relay) => {
                relay.forward(request, responder, deadline);
            }
            None => relay::fail(responder, "relay closed"),
        }
        None
    }

    /// Verifies the Ark's registration after establishing its cloud prerequisites.
    pub(crate) fn genuine(
        &self,
        requester: &Requester,
        deadline: Instant,
    ) -> Result<Registration, Error> {
        let cloud = self.cloud.as_ref().ok_or(Error::MissingEnvironment)?;
        self.sync(requester, deadline)?;
        let proof = requester
            .request(GenuinityProofRequest {}, deadline)?
            .wait::<crate::schema::GenuinityProofResponse>()?;
        cloud.genuine(&proof.proof, deadline).map_err(Into::into)
    }

    /// Exchanges cloud keys and signed time with raw wire requests, bypassing
    /// the prerequisite gate that this exchange is completing.
    fn synchronize(&self, requester: &Requester, deadline: Instant) -> Result<(), Failure> {
        let cloud = self.cloud.as_ref().expect("cloud route available");
        let identity = cloud.identity(deadline)?;
        let started = requester
            .request(identity, deadline)?
            .wait::<crate::schema::CloudSyncStartResponse>()?;
        let time = cloud.time(&started.challenge, deadline)?;
        requester
            .request(time, deadline)?
            .wait::<crate::schema::CloudSyncFinishResponse>()?;
        Ok(())
    }

    /// Ends setup and relay traffic when the owning connection closes.
    pub(crate) fn close(&self) {
        self.end(protocol::Error::Closed);
    }

    /// Retains the wire's original ending reason for setup waiters as well.
    pub(crate) fn end(&self, error: protocol::Error) {
        let mut state = self.state.lock().expect("cloud setup not poisoned");
        if state.error.is_some() {
            return;
        }
        state.error = Some(error.clone());
        for attempt in [state.syncing.take(), state.joining.take()]
            .into_iter()
            .flatten()
        {
            attempt.finish(Err(Failure::Wire(error.clone())));
        }
        if let Some(relay) = state.relay.take() {
            relay.close();
        }
    }
}

/// Prerequisites are initialized independently and always in this order.
#[derive(Clone, Copy)]
enum Step {
    Sync,
    Relay,
}

impl Attempt {
    /// Publishes the first outcome, preserving closure if it won the race.
    fn finish(&self, result: Result<(), Failure>) {
        let mut outcome = self.result.lock().expect("cloud attempt not poisoned");
        if outcome.is_none() {
            *outcome = Some(result);
            self.ready.notify_all();
        }
    }

    /// Waits for this attempt without extending or shortening another caller's budget.
    fn wait(&self, deadline: Instant) -> Result<(), Failure> {
        let mut outcome = self.result.lock().expect("cloud attempt not poisoned");
        loop {
            if let Some(result) = &*outcome {
                return result.clone();
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(protocol::Error::Timeout)?;
            outcome = self
                .ready
                .wait_timeout(outcome, remaining)
                .expect("cloud attempt not poisoned")
                .0;
        }
    }
}

/// Shared setup, retries and connection lifecycle over real wire peers.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::schema::host_to_ark::Content;
    use crate::schema::{
        CloudSyncFinishResponse, CloudSyncStartResponse, DeviceInfoRequest, GenuinityProofResponse,
    };
    use crate::testing::{Peer, answering};
    use crate::trust::Realm;
    use crate::{Ark, TrustMode};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Budget for loopback I/O that is not exercising expiration.
    pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

    /// Serves scripted responses and captures requests, closing each connection
    /// after its response. Accepts are bounded so a failed test leaves no waiter.
    pub(crate) fn serve(responses: Vec<(Duration, String)>) -> (String, mpsc::Receiver<String>) {
        serve_inner(responses, None)
    }

    /// Holds the first response until released, making setup races deterministic.
    fn serve_inner(
        responses: Vec<(Duration, String)>,
        mut pause: Option<mpsc::Receiver<()>>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (delay, response) in responses {
                let deadline = Instant::now() + TIMEOUT;
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => return,
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                let mut request = Vec::new();
                let mut bytes = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    match stream.read(&mut bytes) {
                        Ok(0) | Err(_) => return,
                        Ok(count) => request.extend_from_slice(&bytes[..count]),
                    }
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();
                if let Some(pause) = pause.take()
                    && pause.recv_timeout(TIMEOUT).is_err()
                {
                    return;
                }
                thread::sleep(delay);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, receiver)
    }

    /// Formats a response with a known body length and no persistent connection.
    pub(crate) fn response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        )
    }

    /// Loopback tests ignore ambient proxy settings.
    pub(super) fn http() -> ureq::Agent {
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into()
    }

    /// Attaches a real wire session with cloud routes redirected to the test server.
    pub(crate) fn attach(peer: &mut Peer, url: String) -> Ark {
        let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let services = Arc::new(Services {
            cloud: Some(super::http::tests::api(url, Realm::Hardware)),
            state: Mutex::new(State::default()),
            updating: Mutex::new(()),
        });
        Ark::start(session, services).unwrap()
    }

    /// Peer that only issues a proof after sync, retaining counts for duplicate
    /// initialization checks. Optionally refuses its first start request.
    fn peer(refuse_first: bool) -> (Peer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let proofs = Arc::new(AtomicUsize::new(0));
        let peer = Peer::spawn(Box::new({
            let starts = starts.clone();
            let proofs = proofs.clone();
            let mut synced = false;
            move |session, request, responder| {
                let deadline = Instant::now() + TIMEOUT;
                match request {
                    Content::CloudSyncStart(request) => {
                        assert_eq!(request.signer, [1]);
                        assert_eq!(request.crypto, [2]);
                        if starts.fetch_add(1, Ordering::SeqCst) == 0 && refuse_first {
                            responder
                                .fail(
                                    crate::schema::Error::new(0x111, "identity rejected"),
                                    deadline,
                                )
                                .unwrap()
                                .wait()
                                .unwrap();
                        } else {
                            responder
                                .reply(CloudSyncStartResponse { challenge: vec![3] }, deadline)
                                .unwrap()
                                .wait()
                                .unwrap();
                        }
                    }
                    Content::CloudSyncFinish(request) => {
                        assert_eq!(request.unixmilli, 123);
                        assert_eq!(request.signature, [4]);
                        synced = true;
                        responder
                            .reply(CloudSyncFinishResponse { accepted: 123 }, deadline)
                            .unwrap()
                            .wait()
                            .unwrap();
                    }
                    Content::GenuinityProof(_) => {
                        assert!(synced, "proof requested before cloud sync");
                        proofs.fetch_add(1, Ordering::SeqCst);
                        responder
                            .reply(
                                GenuinityProofResponse {
                                    proof: vec![0xfb, 0xff],
                                },
                                deadline,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                    }
                    Content::SlotList(_) => {
                        assert!(synced, "slots requested before cloud sync");
                        responder
                            .reply(crate::schema::SlotListResponse::default(), deadline)
                            .unwrap()
                            .wait()
                            .unwrap();
                    }
                    request => return answering(session, request, responder),
                }
                true
            }
        }));
        (peer, starts, proofs)
    }

    /// JSON replies used by the real wire peer's cloud synchronization exchange.
    fn sync_responses() -> Vec<(Duration, String)> {
        vec![
            (
                Duration::ZERO,
                response(200, r#"{"signer":"AQ==","crypto":"Ag=="}"#),
            ),
            (
                Duration::ZERO,
                response(200, r#"{"unixmilli":123,"signature":"BA=="}"#),
            ),
        ]
    }

    /// Concurrent clients share setup, status bypasses it, short waiters expire
    /// independently and subsequent operations reuse the successful exchange.
    #[test]
    fn test_shared_setup() {
        let mut responses = sync_responses();
        responses.push((Duration::ZERO, response(200, r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#)));
        let (release, pause) = mpsc::channel();
        let (url, requests) = serve_inner(responses, Some(pause));
        let (mut peer, starts, proofs) = peer(false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        requests.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, deadline)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(matches!(
            client
                .clone()
                .call_timeout(GenuinityProofRequest {}, Duration::from_millis(20)),
            Err(Error::Timeout)
        ));
        let follower = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        release.send(()).unwrap();
        leader.join().unwrap().unwrap();
        follower.join().unwrap().unwrap();
        assert!(client.genuine(deadline).unwrap().active());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(proofs.load(Ordering::SeqCst), 3);
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .starts_with("GET /v1/cloudsync/time?challenge=03 ")
        );
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .starts_with("GET /v1/genuine ")
        );
    }

    /// A refused attempt preserves its device error and does not poison a retry.
    #[test]
    fn test_setup_retry() {
        let mut responses = sync_responses();
        responses.insert(0, responses[0].clone());
        let (url, _requests) = serve(responses);
        let (mut peer, starts, proofs) = peer(true);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        assert!(matches!(client.call(GenuinityProofRequest {}, deadline),
            Err(Error::Remote(error)) if error.code == 0x111));
        client.call(GenuinityProofRequest {}, deadline).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(proofs.load(Ordering::SeqCst), 1);
    }

    /// Closing releases setup waiters before the outstanding HTTP response arrives
    /// and prevents that response from starting any request on the closed Ark.
    #[test]
    fn test_close_during_setup() {
        let (release, pause) = mpsc::channel();
        let (url, requests) = serve_inner(vec![sync_responses().remove(0)], Some(pause));
        let (mut peer, starts, _) = peer(false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        requests.recv_timeout(TIMEOUT).unwrap();
        let waiter = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        ark.closer().close();
        assert!(matches!(waiter.join().unwrap(), Err(Error::Closed)));
        release.send(()).unwrap();
        assert!(matches!(leader.join().unwrap(), Err(Error::Closed)));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    /// Self-signed and recovery sessions retain local operations. Cloud-dependent
    /// requests fail clearly when the caller did not supply an environment.
    #[test]
    fn test_unattested_setup() {
        for recover in [false, true] {
            let mut peer = Peer::spawn(Box::new(answering));
            let policy = if recover {
                TrustMode::Recover(Box::new(peer.identity.clone()))
            } else {
                TrustMode::RootOrSelf
            };
            let (ark, _) = Ark::attach(peer.stream(), &policy, None).unwrap();
            let client = ark.client();
            assert!(matches!(
                client.call(GenuinityProofRequest {}, Instant::now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            assert!(matches!(
                client.genuine(Instant::now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            assert!(matches!(
                client.call(crate::schema::UnlockRequest {}, Instant::now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            client
                .call(DeviceInfoRequest {}, Instant::now() + TIMEOUT)
                .unwrap();
        }
    }

    /// Explicit routing lets self-signed and recovery peers sync and query slots.
    /// Registry authentication uses their opaque proofs without an attested serial.
    #[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
    #[test]
    fn test_unattested_cloud() {
        for recover in [false, true] {
            for realm in [Realm::Hardware, Realm::Emulator] {
                let mut responses = sync_responses();
                responses.push((Duration::ZERO, response(200, r#"{"serial":"registry-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#)));
                responses.push((Duration::ZERO, response(403, "registry refused proof")));
                let (url, requests) = serve(responses);
                let (mut peer, starts, proofs) = peer(false);
                let policy = if recover {
                    TrustMode::Recover(Box::new(peer.identity.clone()))
                } else {
                    TrustMode::RootOrSelf
                };
                let (session, identity) = protocol::connect(peer.stream(), &policy).unwrap();
                assert_eq!(identity.realm(), None);
                assert_eq!(matches!(identity, Identity::Recovered(_)), recover);

                let env = crate::identity::ENVIRONMENTS[0];
                let mut services = Services::new(&identity, Some((env, realm)));
                let cloud = services.cloud.as_mut().unwrap();
                cloud.url = url;
                cloud.agent = http();
                let ark = Ark::start(session, Arc::new(services)).unwrap();
                let client = ark.client();
                let deadline = Instant::now() + TIMEOUT;

                client.call(DeviceInfoRequest {}, deadline).unwrap();
                assert_eq!(starts.load(Ordering::SeqCst), 0);
                for _ in 0..2 {
                    client
                        .call(crate::schema::SlotListRequest {}, deadline)
                        .unwrap();
                }
                let registration = client.genuine(deadline).unwrap();
                assert_eq!(registration.serial, "registry-serial");
                assert!(registration.active());
                assert!(matches!(client.genuine(deadline), Err(Error::Cloud(_))));
                assert_eq!(starts.load(Ordering::SeqCst), 1);
                assert_eq!(proofs.load(Ordering::SeqCst), 2);

                let registry = match realm {
                    Realm::Hardware => "/v1/genuine",
                    Realm::Emulator => "/v1/sandbox/genuine",
                };
                for path in [
                    "/v1/cloudsync/identity",
                    "/v1/cloudsync/time?challenge=03",
                    registry,
                    registry,
                ] {
                    let request = requests.recv_timeout(TIMEOUT).unwrap();
                    assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                }
            }
        }
    }
}
