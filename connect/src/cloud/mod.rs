// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Cloud prerequisites and registry checks for an Ark connection.
//!
//! Clients share one setup state per wire session. The caller that starts an
//! attempt performs its I/O; concurrent callers wait on that attempt with their
//! own deadlines. Only successful setup is reused. Closure releases waiters,
//! while an outstanding blocking I/O retains the initiating caller's deadline.

mod auth;
mod dns;
mod firmware;
mod http;
mod pairing;
pub use pairing::PairingProgress;
mod relay;
mod socket;

pub use auth::CloudAuth;
pub use firmware::{Firmware, UpdateProgress};
pub use http::Registration;

use crate::schema::{
    GenuinityProofRequest, RelayArkToAppRequest, RelayJoinRequest, RelayJoinResponse,
};
use crate::{Error, Identity, Timing};
use darkbio_wire::protocol::{self, Requester, Responder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Whether the reported cloud identity and clock can be reused for a request.
/// Sync is needed before the first use or at 15 seconds of clock drift, leaving
/// headroom for proof verification. Key rotation is detected by a refused proof;
/// the marker only records setup since boot.
pub fn cloud_synced(info: &crate::schema::DeviceInfoResponse) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    synced_at(info, now)
}

/// Compares the boot marker and device clock with host time in Unix seconds.
fn synced_at(info: &crate::schema::DeviceInfoResponse, now: u64) -> bool {
    info.cloud_synced && info.cloud_clock.abs_diff(now) < 15
}

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
    synced: Option<(Instant, bool)>, // Last sync decision and when it was observed
    error: Option<protocol::Error>,  // Why the owning wire session ended
    syncing: Option<Arc<Attempt>>,   // Cloud sync joined by concurrent callers
    relay: Option<relay::Relay>,     // Relay attached lazily to this connection
    joining: Option<Arc<Attempt>>,   // Relay attachment joined by concurrent callers
}

/// One attempt's outcome, retained by its waiters even after a retry starts.
#[derive(Debug, Default)]
struct Attempt {
    result: Mutex<Option<Result<(), Failure>>>, // Shared success or the original failure
    refreshed: AtomicBool,                      // This attempt exchanged keys and signed time
    ready: Condvar,                             // Wakes waiters on completion or closure
}

/// Failures shareable between callers joining the same initialization attempt.
#[derive(Clone, Debug)]
enum Failure {
    /// The identity and caller supplied no route for cloud operations.
    MissingEnvironment,
    /// The cloud refused the Ark's proof, possibly after a key rotation.
    ProofRejected,
    /// Caller authentication was rejected before reaching the cloud application.
    AuthRequired,
    /// Login failed or the caller cannot prompt for it.
    CloudAuth {
        origin: String,  // Selected host whose credentials need attention
        message: String, // Safe diagnostic shared with setup waiters
    },
    Cloud(String),         // HTTP or response decoding failure
    Relay(String),         // Relay connection or envelope failure
    Wire(protocol::Error), // Device failure, retaining remote codes and disconnect reasons
}

impl From<String> for Failure {
    /// Retains a cloud decoding or validation diagnostic for all setup waiters.
    fn from(error: String) -> Self {
        Self::Cloud(error)
    }
}

impl From<protocol::Error> for Failure {
    /// Preserves wire's typed refusal, timeout or session ending reason.
    fn from(error: protocol::Error) -> Self {
        Self::Wire(error)
    }
}

impl From<Failure> for Error {
    /// Restores the public error category after a shared attempt completes.
    fn from(error: Failure) -> Self {
        match error {
            Failure::MissingEnvironment => Self::MissingEnvironment,
            Failure::ProofRejected => Self::ProofRejected,
            Failure::AuthRequired => Self::Cloud("cloud access still refused after login".into()),
            Failure::CloudAuth { origin, message } => Self::CloudAuth { origin, message },
            Failure::Cloud(error) => Self::Cloud(error),
            Failure::Relay(error) => Self::Relay(error),
            Failure::Wire(error) => error.into(),
        }
    }
}

impl Services {
    /// Installs caller-owned credentials without contacting the selected cloud.
    pub(crate) fn set_cloud_auth(&self, auth: Arc<dyn CloudAuth>) {
        if let Some(cloud) = &self.cloud {
            cloud.auth.set(auth);
        }
    }
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

    /// Reuses fresh device state, caching that decision for one minute. Each
    /// caller bounds its own wait; only one exchange runs at a time.
    pub(crate) fn sync(
        &self,
        requester: &Requester,
        timing: impl Into<Timing>,
    ) -> Result<(), Error> {
        self.ensure(requester, Step::Sync, timing.into())
            .map_err(Into::into)
    }

    /// An explicit diagnostic refresh invalidates the reused setup, joining an
    /// exchange already in flight if another caller is synchronizing.
    pub(crate) fn resync(&self, requester: &Requester, timing: Timing) -> Result<(), Error> {
        self.ensure(requester, Step::Refresh, timing)
            .map_err(Into::into)
    }

    /// Attaches the relay after cloud sync, reusing a healthy connection. A
    /// failed relay is replaced on the next call without replaying any operation.
    pub(crate) fn relay(
        &self,
        requester: &Requester,
        timing: impl Into<Timing>,
    ) -> Result<(), Error> {
        let timing = timing.into();
        self.sync(requester, timing)?;
        self.ensure(requester, Step::Relay, timing)
            .map_err(Into::into)
    }

    /// Serializes one prerequisite while allowing unrelated device traffic.
    /// Waiters retain the attempt they joined, even if a later caller retries it.
    fn ensure(&self, requester: &Requester, step: Step, timing: Timing) -> Result<(), Failure> {
        let deadline = timing.io();
        let (attempt, leader) = {
            let mut state = self.state.lock().expect("cloud setup not poisoned");
            if let Some(error) = &state.error {
                return Err(error.clone().into());
            }
            match step {
                _ if self.cloud.is_none() => return Err(Failure::MissingEnvironment),
                Step::Sync
                    if state.synced.is_some_and(|(at, synced)| {
                        synced && at.elapsed() < Duration::from_secs(60)
                    }) =>
                {
                    return Ok(());
                }
                Step::Relay if state.relay.as_ref().is_some_and(relay::Relay::connected) => {
                    return Ok(());
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(protocol::Error::Timeout.into());
            }
            if matches!(step, Step::Refresh) {
                state.synced = None;
            }
            let pending = match step {
                Step::Sync | Step::Refresh => &mut state.syncing,
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
            attempt.wait(deadline)?;
            // A joined freshness check may have reused device state. An explicit
            // refresh still needs an exchange that actually replaces cloud keys.
            return if matches!(step, Step::Refresh) && !attempt.refreshed.load(Ordering::Acquire) {
                self.ensure(requester, step, timing)
            } else {
                Ok(())
            };
        }
        // Run setup without the state lock, then publish only if the session
        // remains open. Closure wins over a late successful network response.
        let result = match step {
            Step::Sync | Step::Refresh => self
                .synchronize(requester, timing, matches!(step, Step::Refresh))
                .map(|refreshed| {
                    attempt.refreshed.store(refreshed, Ordering::Release);
                    None
                }),
            Step::Relay => self.join(requester, timing).map(Some),
        };
        let mut state = self.state.lock().expect("cloud setup not poisoned");
        let result = if let Some(error) = &state.error {
            Err(Failure::Wire(error.clone()))
        } else {
            result.and_then(|relay| {
                match step {
                    Step::Sync | Step::Refresh => {
                        state.synced = Some((Instant::now(), true));
                    }
                    Step::Relay => {
                        let mut relay = relay.expect("relay setup returned an attachment");
                        relay.start()?;
                        state.relay = Some(relay);
                        tracing::info!(target: "darkbio_connect::setup", "relay attached");
                    }
                }
                Ok(())
            })
        };
        match step {
            Step::Sync | Step::Refresh => state.syncing = None,
            Step::Relay => state.joining = None,
        }
        attempt.finish(result.clone());
        result
    }

    /// Authenticates relay attachment with a fresh authorization from the Ark.
    fn join(&self, requester: &Requester, timing: Timing) -> Result<relay::Relay, Failure> {
        let cloud = self.cloud.as_ref().expect("cloud route available");
        self.authenticate(requester, timing, || {
            let joined = requester
                .request(RelayJoinRequest {}, timing.io())?
                .wait::<RelayJoinResponse>()?;
            relay::Relay::connect(
                cloud,
                &cloud.relay_url(),
                &joined.auth,
                requester.clone(),
                timing.io(),
            )
        })
    }

    /// A cloud key can rotate while the Ark still reports sync. Refresh once
    /// after a refused proof, then obtain a new proof for the same authentication.
    /// Callers must stop here before any pairing, approval or transfer begins.
    fn authenticate<T>(
        &self,
        requester: &Requester,
        timing: Timing,
        mut attempt: impl FnMut() -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let cloud = self.cloud.as_ref().ok_or(Failure::MissingEnvironment)?;
        let result = cloud.with_auth(timing, &mut attempt);
        if matches!(result, Err(Failure::ProofRejected)) {
            self.ensure(requester, Step::Refresh, timing)?;
            return cloud.with_auth(timing, attempt);
        }
        result
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
        timing: Timing,
    ) -> Result<Registration, Error> {
        let cloud = self.cloud.as_ref().ok_or(Error::MissingEnvironment)?;
        self.sync(requester, timing)?;
        self.authenticate(requester, timing, || {
            let proof = requester
                .request(GenuinityProofRequest {}, timing.io())?
                .wait::<crate::schema::GenuinityProofResponse>()?;
            cloud.genuine(&proof.proof, timing.io())
        })
        .map_err(Into::into)
    }

    /// Exchanges cloud keys and signed time with raw wire requests, bypassing
    /// the prerequisite gate that this exchange is completing.
    fn synchronize(
        &self,
        requester: &Requester,
        timing: Timing,
        force: bool,
    ) -> Result<bool, Failure> {
        if !force {
            let reported = self
                .state
                .lock()
                .expect("cloud setup not poisoned")
                .synced
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(60))
                .map(|(_, synced)| synced);
            let synced = match reported {
                Some(synced) => synced,
                None => self.synced(requester, timing)?,
            };
            if synced {
                tracing::debug!(target: "darkbio_connect::setup", "reusing device cloud synchronization");
                return Ok(false);
            }
        }
        let cloud = self.cloud.as_ref().expect("cloud route available");
        let identity = cloud.with_auth(timing, || cloud.identity(timing.io()))?;
        let started = requester
            .request(identity, timing.io())?
            .wait::<crate::schema::CloudSyncStartResponse>()?;
        let time = cloud.with_auth(timing, || cloud.time(&started.challenge, timing.io()))?;
        requester
            .request(time, timing.io())?
            .wait::<crate::schema::CloudSyncFinishResponse>()?;
        tracing::info!(target: "darkbio_connect::setup", "cloud synchronized");
        Ok(true)
    }

    /// Checks device state without a cloud request or a cached host decision.
    pub(crate) fn synced(
        &self,
        requester: &Requester,
        timing: Timing,
    ) -> Result<bool, protocol::Error> {
        let info = requester
            .request(crate::schema::DeviceInfoRequest {}, timing.io())?
            .wait::<crate::schema::DeviceInfoResponse>()?;
        Ok(cloud_synced(&info))
    }

    /// Reuses device info already requested by the caller. A delayed response
    /// must not overwrite a newer observation or an active sync exchange.
    pub(crate) fn reported(&self, info: &crate::schema::DeviceInfoResponse, requested: Instant) {
        let mut state = self.state.lock().expect("cloud setup not poisoned");
        if state.error.is_none()
            && state.syncing.is_none()
            && state.synced.is_none_or(|(at, _)| at < requested)
        {
            state.synced = Some((requested, cloud_synced(info)));
        }
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

/// Setup action to join or start. Relay callers establish cloud sync first.
#[derive(Clone, Copy)]
enum Step {
    /// Reuse fresh device state or exchange cloud keys and signed time.
    Sync,
    /// Exchange keys and time even if the device reports usable setup.
    Refresh,
    /// Authenticate and start a relay worker, reusing a healthy attachment.
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
    pub(super) fn sync_responses() -> Vec<(Duration, String)> {
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
    #[test]
    fn test_unattested_cloud() {
        for recover in [false, true] {
            for realm in [Realm::Hardware, Realm::Emulator] {
                let mut responses = sync_responses();
                responses.push((Duration::ZERO, response(200, r#"{"serial":"registry-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#)));
                responses.push((Duration::ZERO, response(403, "registry refused proof")));
                responses.extend(sync_responses());
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
                assert!(matches!(
                    client.genuine(deadline),
                    Err(Error::ProofRejected)
                ));
                assert_eq!(starts.load(Ordering::SeqCst), 2);
                assert_eq!(proofs.load(Ordering::SeqCst), 3);

                let registry = match realm {
                    Realm::Hardware => "/v1/genuine",
                    Realm::Emulator => "/v1/sandbox/genuine",
                };
                for path in [
                    "/v1/cloudsync/identity",
                    "/v1/cloudsync/time?challenge=03",
                    registry,
                    registry,
                    "/v1/cloudsync/identity",
                    "/v1/cloudsync/time?challenge=03",
                    registry,
                ] {
                    let request = requests.recv_timeout(TIMEOUT).unwrap();
                    assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                }
            }
        }
    }

    #[test]
    fn test_sync_freshness() {
        use crate::schema::DeviceInfoResponse;
        for (synced, clock, expected) in [
            (false, 0, false),
            (false, 1000, false),
            (true, 0, false),
            (true, 1000, true),
            (true, 985, false),
            (true, 986, true),
            (true, 1014, true),
            (true, 1015, false),
        ] {
            assert_eq!(
                synced_at(
                    &DeviceInfoResponse {
                        cloud_synced: synced,
                        cloud_clock: clock,
                        ..Default::default()
                    },
                    1000
                ),
                expected,
                "synced {synced}, clock {clock}"
            );
        }
    }

    /// A refused authentication refreshes the cloud identity and obtains a new
    /// proof once. Other HTTP failures and a second refusal keep their errors.
    #[test]
    fn test_authentication_refresh() {
        use crate::schema;
        for operation in ["genuine", "relaying", "pairing"] {
            for (status, retried) in [(400, 400), (403, 403), (403, 200), (503, 503)] {
                if retried == 200 && operation != "genuine" {
                    continue;
                }
                let mut responses = vec![(Duration::ZERO, response(status, "refused"))];
                if status == 403 {
                    responses.extend(sync_responses());
                    responses.push((Duration::ZERO, response(retried, if retried == 200 {
                        r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#
                    } else { "still refused" })));
                }
                let (url, requests) = serve(responses);
                let proofs = Arc::new(AtomicUsize::new(0));
                let mut peer = Peer::spawn(Box::new({
                    let proofs = proofs.clone();
                    let mut refreshed = false;
                    move |_, request, responder| {
                        let deadline = Instant::now() + TIMEOUT;
                        let proof = vec![u8::from(refreshed)];
                        let response: protocol::Message = match request {
                            Content::DeviceInfo(_) => schema::DeviceInfoResponse {
                                cloud_synced: true,
                                cloud_clock: SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs(),
                                ..Default::default()
                            }
                            .into(),
                            Content::CloudSyncStart(_) => {
                                schema::CloudSyncStartResponse { challenge: vec![3] }.into()
                            }
                            Content::CloudSyncFinish(_) => {
                                refreshed = true;
                                schema::CloudSyncFinishResponse { accepted: 123 }.into()
                            }
                            Content::GenuinityProof(_) => {
                                proofs.fetch_add(1, Ordering::SeqCst);
                                schema::GenuinityProofResponse { proof }.into()
                            }
                            Content::RelayJoin(_) => {
                                proofs.fetch_add(1, Ordering::SeqCst);
                                schema::RelayJoinResponse { auth: proof }.into()
                            }
                            Content::PairingAuth(_) => {
                                proofs.fetch_add(1, Ordering::SeqCst);
                                schema::PairingAuthResponse {
                                    auth: proof,
                                    fprint: vec![8; 32],
                                }
                                .into()
                            }
                            other => panic!("unexpected request: {other:?}"),
                        };
                        responder.reply(response, deadline).unwrap();
                        true
                    }
                }));
                let ark = attach(&mut peer, url);
                let client = ark.client();
                let deadline = Instant::now() + TIMEOUT;
                let result = match operation {
                    "genuine" => client.genuine(deadline).map(drop),
                    "relaying" => client.attach_relay(deadline),
                    "pairing" => {
                        client.pair(deadline, |_| panic!("pairing began before authentication"))
                    }
                    _ => unreachable!(),
                };
                if retried == 200 {
                    result.unwrap();
                } else if status == 403 {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::ProofRejected), "{error:?}");
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Cloud(_)), "{error:?}");
                }
                let requests: Vec<_> = requests.try_iter().collect();
                assert!(requests[0].starts_with(&format!("GET /v1/{operation} ")));
                assert_eq!(
                    proofs.load(Ordering::SeqCst),
                    if status == 403 { 2 } else { 1 }
                );
                assert_eq!(requests.len(), if status == 403 { 4 } else { 1 });
                if status == 403 {
                    assert!(requests[1].contains("/cloudsync/identity"));
                    assert!(requests[2].contains("/cloudsync/time"));
                    assert!(requests[3].starts_with(&format!("GET /v1/{operation} ")));
                    let auth = if operation == "genuine" {
                        "dark-auth: "
                    } else {
                        "Dark-Auth|"
                    };
                    assert!(
                        requests[0].contains(&format!("{auth}AA")),
                        "{}",
                        requests[0]
                    );
                    assert!(
                        requests[3].contains(&format!("{auth}AQ")),
                        "{}",
                        requests[3]
                    );
                }
            }
        }
    }

    #[test]
    fn caller_authentication_retries_fresh_proofs_without_cloud_sync() {
        use auth::tests::{Login, refused};
        for operation in ["genuine", "relaying", "pairing"] {
            for fail_login in [false, true] {
                let mut responses = vec![(Duration::ZERO, refused(403))];
                if !fail_login {
                    responses.push((Duration::ZERO, if operation == "genuine" {
                        response(200, r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#)
                    } else { refused(403) }));
                }
                let (url, requests) = serve(responses);
                let proofs = Arc::new(AtomicUsize::new(0));
                let mut peer = Peer::spawn(Box::new({
                    let proofs = proofs.clone();
                    move |_, request, responder| {
                        let response: protocol::Message = match request {
                            Content::DeviceInfo(_) => crate::schema::DeviceInfoResponse {
                                cloud_synced: true,
                                cloud_clock: SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs(),
                                ..Default::default()
                            }
                            .into(),
                            Content::GenuinityProof(_) => crate::schema::GenuinityProofResponse {
                                proof: vec![proofs.fetch_add(1, Ordering::SeqCst) as u8],
                            }
                            .into(),
                            Content::RelayJoin(_) => crate::schema::RelayJoinResponse {
                                auth: vec![proofs.fetch_add(1, Ordering::SeqCst) as u8],
                            }
                            .into(),
                            Content::PairingAuth(_) => crate::schema::PairingAuthResponse {
                                auth: vec![proofs.fetch_add(1, Ordering::SeqCst) as u8],
                                fprint: vec![8; 32],
                            }
                            .into(),
                            other => panic!("unexpected request: {other:?}"),
                        };
                        responder.reply(response, Instant::now() + TIMEOUT).unwrap();
                        true
                    }
                }));
                let mut ark = attach(&mut peer, url);
                let login = Login {
                    fail: fail_login,
                    ..Default::default()
                };
                ark.set_cloud_auth(login.clone());
                let client = ark.client();
                let timing = Timing::inactivity(TIMEOUT);
                client.call(DeviceInfoRequest {}, timing).unwrap();
                assert_eq!(login.lookups.load(Ordering::SeqCst), 0);
                let result = match operation {
                    "genuine" => client.genuine(timing).map(drop),
                    "relaying" => client.attach_relay(timing),
                    "pairing" => {
                        client.pair(timing, |_| panic!("pairing started before authentication"))
                    }
                    _ => unreachable!(),
                };
                if operation == "genuine" && !fail_login {
                    result.unwrap();
                } else {
                    assert!(matches!(result, Err(Error::CloudAuth { .. })), "{result:?}");
                }
                let requests: Vec<_> = requests.try_iter().collect();
                assert_eq!(requests.len(), if fail_login { 1 } else { 2 });
                assert_eq!(proofs.load(Ordering::SeqCst), requests.len());
                assert_eq!(login.logins.load(Ordering::SeqCst), 1);
                assert_eq!(login.lookups.load(Ordering::SeqCst), 1);
                for (i, request) in requests.iter().enumerate() {
                    let request = request.to_ascii_lowercase();
                    assert!(request.starts_with(&format!("get /v1/{operation} ")));
                    assert!(request.contains(if i == 0 {
                        "authorization: cached\r\n"
                    } else {
                        "authorization: refreshed\r\n"
                    }));
                    let auth = if operation == "genuine" {
                        "dark-auth: "
                    } else {
                        "dark-auth|"
                    };
                    assert!(
                        request.contains(&format!("{auth}{}", if i == 0 { "aa" } else { "aq" }))
                    );
                }
            }
        }
    }

    #[test]
    fn cloud_sync_logs_in_before_sending_certificates_to_the_ark() {
        use auth::tests::{Login, refused};
        let mut responses = vec![(Duration::ZERO, refused(302))];
        responses.extend(sync_responses());
        let (url, requests) = serve(responses);
        let (mut peer, starts, _) = peer(false);
        let mut ark = attach(&mut peer, url);
        let login = Login::default();
        ark.set_cloud_auth(login.clone());
        ark.client().sync(Timing::inactivity(TIMEOUT)).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(login.logins.load(Ordering::SeqCst), 1);
        let requests: Vec<_> = requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("authorization: cached\r\n"));
        assert!(requests[1].contains("authorization: refreshed\r\n"));
        assert!(requests[2].contains("authorization: refreshed\r\n"));
    }

    /// Reusing device state needs no HTTP, while explicit diagnostics always
    /// refresh it. Dataset paths keep every field across the connection.
    #[test]
    fn test_reported_sync_and_explicit_refresh() {
        use crate::schema;
        let expected = schema::DatasetPathsResponse {
            paths: vec![schema::DatasetPath {
                path: "v1/sample/<item>".into(),
                directory: true,
                grantable: true,
                available: true,
                desc: "A sample item.".into(),
                format: "Plain text.".into(),
                examples: vec!["first".into(), "second".into()],
            }],
        };
        for initially_synced in [false, true] {
            let mut responses = sync_responses();
            if !initially_synced {
                responses.extend(sync_responses());
            }
            let (url, requests) = serve(responses);
            let infos = Arc::new(AtomicUsize::new(0));
            let mut peer = Peer::spawn(Box::new({
                let infos = infos.clone();
                let paths = expected.clone();
                let mut synced = initially_synced;
                move |session, request, responder| {
                    let deadline = Instant::now() + TIMEOUT;
                    let response: protocol::Message = match request {
                        Content::DeviceInfo(_) => {
                            infos.fetch_add(1, Ordering::SeqCst);
                            let clock = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            schema::DeviceInfoResponse {
                                cloud_clock: clock,
                                cloud_synced: synced,
                                ..Default::default()
                            }
                            .into()
                        }
                        Content::DatasetPaths(_) => {
                            assert!(synced, "request served before sync");
                            paths.clone().into()
                        }
                        Content::CloudSyncStart(_) => {
                            schema::CloudSyncStartResponse { challenge: vec![3] }.into()
                        }
                        Content::CloudSyncFinish(_) => {
                            synced = true;
                            schema::CloudSyncFinishResponse { accepted: 123 }.into()
                        }
                        other => return answering(session, other, responder),
                    };
                    responder.reply(response, deadline).unwrap();
                    true
                }
            }));
            let ark = attach(&mut peer, url);
            let client = ark.client();
            let deadline = Instant::now() + TIMEOUT;
            let info = client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
            assert_eq!(info.cloud_synced, initially_synced);
            for _ in 0..2 {
                let paths = client
                    .call(schema::DatasetPathsRequest {}, deadline)
                    .unwrap();
                assert_eq!(paths, expected);
            }
            assert_eq!(infos.load(Ordering::SeqCst), 1);
            assert_eq!(
                requests.try_iter().count(),
                if initially_synced { 0 } else { 2 }
            );
            client.sync(deadline).unwrap();
            assert!(
                requests
                    .recv_timeout(TIMEOUT)
                    .unwrap()
                    .contains("/cloudsync/identity")
            );
            assert!(
                requests
                    .recv_timeout(TIMEOUT)
                    .unwrap()
                    .contains("/cloudsync/time")
            );
            assert_eq!(infos.load(Ordering::SeqCst), 1);
        }
    }

    /// Waiting on an older status response must not undo a completed refresh.
    #[test]
    fn test_delayed_device_info_retains_newer_sync() {
        let (url, requests) = serve(sync_responses());
        let (mut peer, starts, _) = peer(false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        let pending = client.send(DeviceInfoRequest {}, deadline).unwrap();
        client.sync(deadline).unwrap();
        assert!(!pending.wait().unwrap().cloud_synced);
        client
            .call(crate::schema::SlotListRequest {}, deadline)
            .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(requests.try_iter().count(), 2);
    }

    /// A core restart can invalidate setup without losing the wire session.
    /// Retry only its reserved refusal and only with evidence of lost sync.
    #[test]
    fn test_unavailable_retry_requires_lost_sync() {
        use crate::schema::{self, ReservedErrors};
        for (code, reset, repeat, retries) in [
            (ReservedErrors::Unavailable as u64, true, false, 1),
            (ReservedErrors::Unavailable as u64, true, true, 1),
            (ReservedErrors::Unavailable as u64, false, false, 0),
            (ReservedErrors::Unauthorized as u64, true, false, 0),
            (0x1234, true, false, 0),
        ] {
            let (url, requests) = serve(if retries == 1 {
                sync_responses()
            } else {
                vec![]
            });
            let count = Arc::new(AtomicUsize::new(0));
            let mut peer = Peer::spawn(Box::new({
                let count = count.clone();
                let mut synced = true;
                move |session, request, responder| {
                    let deadline = Instant::now() + TIMEOUT;
                    let response: protocol::Message = match request {
                        Content::DeviceInfo(_) => {
                            let clock = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            schema::DeviceInfoResponse {
                                cloud_clock: clock,
                                cloud_synced: synced,
                                ..Default::default()
                            }
                            .into()
                        }
                        Content::SlotList(_) => {
                            let attempt = count.fetch_add(1, Ordering::SeqCst);
                            if attempt == 0 || repeat {
                                if reset {
                                    synced = false;
                                }
                                responder
                                    .fail(schema::Error::new(code, "original refusal"), deadline)
                                    .unwrap();
                                return true;
                            }
                            schema::SlotListResponse::default().into()
                        }
                        Content::CloudSyncStart(_) => {
                            schema::CloudSyncStartResponse { challenge: vec![3] }.into()
                        }
                        Content::CloudSyncFinish(_) => {
                            synced = true;
                            schema::CloudSyncFinishResponse { accepted: 123 }.into()
                        }
                        other => return answering(session, other, responder),
                    };
                    responder.reply(response, deadline).unwrap();
                    true
                }
            }));
            let ark = attach(&mut peer, url);
            let result = ark
                .client()
                .call(schema::SlotListRequest {}, Instant::now() + TIMEOUT);
            assert_eq!(count.load(Ordering::SeqCst), 1 + retries);
            if retries == 1 && !repeat {
                assert!(result.is_ok());
            } else {
                assert!(
                    matches!(result, Err(Error::Remote(error)) if error.code == code && error.msg == "original refusal")
                );
            }
            assert_eq!(requests.try_iter().count(), 2 * retries);
        }
    }
}
