// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Cloud services of an Ark connection, from setup and registry checks to
//! relaying, pairing and firmware updates.
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
use darkbio_clock::{Clock, sync};
use darkbio_wire::protocol::{self, Requester, Responder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Checks whether the cloud setup that a
/// [`DeviceInfoResponse`](crate::schema::DeviceInfoResponse) reports can be
/// reused for a request.
///
/// Sync is needed when the Ark reports no setup since boot, or when its clock
/// is 15 s or more away from the wall time of the [`Clock`]. That bound leaves
/// headroom for proof verification. The marker records only setup since boot,
/// so it cannot reveal changed cloud keys.
pub fn cloud_synced(info: &crate::schema::DeviceInfoResponse, clock: &Clock) -> bool {
    let now = clock
        .system_time()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    synced_at(info, now)
}

/// Checks the boot marker and compares the device clock with `now`, the host
/// time in Unix seconds.
fn synced_at(info: &crate::schema::DeviceInfoResponse, now: u64) -> bool {
    info.cloud_synced && info.cloud_clock.abs_diff(now) < 15
}

/// Cloud setup shared by every client of one connection.
///
/// Network and device I/O run outside its lock, so independent requests and
/// closure stay available.
#[derive(Debug)]
pub(crate) struct Services {
    /// Clock of the wire session, which every operation reads.
    clock: Clock,
    /// Cloud API client, absent when neither the attestation nor the caller
    /// selects an environment.
    cloud: Option<http::Api>,
    /// Setup attempts in flight, their reusable results and the session's
    /// ending reason.
    state: Mutex<State>,
    /// Lock admitting one firmware update at a time across client clones.
    updating: Mutex<()>,
}

/// Setup attempts, results and ending reason of one connection.
///
/// Each step runs one attempt at a time and becomes reusable only on success.
#[derive(Debug, Default)]
struct State {
    /// Last sync decision and when it was observed.
    synced: Option<(Instant, bool)>,
    /// Reason the owning wire session ended, once it has.
    error: Option<protocol::Error>,
    /// Cloud sync attempt in flight, which concurrent callers join.
    syncing: Option<Arc<Attempt>>,
    /// Relay attached to this connection, once a request needs it.
    relay: Option<relay::Relay>,
    /// Relay attachment in flight, which concurrent callers join.
    joining: Option<Arc<Attempt>>,
}

/// One attempt's outcome, retained by its waiters even after a retry starts.
#[derive(Debug)]
struct Attempt {
    /// Outcome shared by every waiter, set once.
    result: sync::Mutex<Option<Result<(), Failure>>>,
    /// Whether this attempt exchanged keys and signed time with the cloud.
    refreshed: AtomicBool,
    /// Signal that wakes the waiters on completion or closure.
    ready: sync::Condvar,
}

/// Failures shareable between callers joining the same initialization attempt.
#[derive(Clone, Debug)]
enum Failure {
    /// Neither the attestation nor the caller selected a cloud environment.
    MissingEnvironment,
    /// The cloud answered a request carrying the Ark's proof with HTTP 403.
    ProofRejected,
    /// The caller's provider recognized a response as refusing its credentials.
    AuthRequired,
    /// A login failed, no provider could log in, or the host refused even fresh
    /// credentials.
    CloudAuth {
        /// Origin of the host whose credentials need attention.
        origin: String,
        /// Diagnostic without credentials, shared with setup waiters.
        message: String,
    },
    /// A cloud request or socket failed, or its response could not be used.
    Cloud(String),
    /// The relay ended, or an exchange on it broke the protocol or its limits.
    Relay(String),
    /// A wire request failed or an operation timed out, keeping the Ark's
    /// refusal or the disconnect reason.
    Wire(protocol::Error),
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
    /// Installs the caller's authentication provider without contacting the
    /// cloud, doing nothing without a cloud route.
    pub(crate) fn set_cloud_auth(&self, auth: Arc<dyn CloudAuth>) {
        if let Some(cloud) = &self.cloud {
            cloud.auth.set(auth);
        }
    }

    /// Converts a request error, replacing a bare closure with the session's
    /// recorded ending reason.
    ///
    /// A stopped dispatcher drops its wire session, after which a surviving
    /// requester can only report it closed.
    pub(crate) fn wire_error(&self, error: protocol::Error) -> Error {
        if matches!(error, protocol::Error::Closed)
            && let Some(ended) = &self.state.lock().expect("cloud setup not poisoned").error
        {
            return ended.clone().into();
        }
        error.into()
    }

    /// Records cloud routing for a connection without starting network I/O.
    ///
    /// Every operation of the connection reads its time from `clock`, the wire
    /// session's clock.
    pub(crate) fn new(
        identity: &Identity,
        cloud: Option<(crate::trust::Environment, crate::trust::Realm)>,
        clock: &Clock,
    ) -> Self {
        Self {
            clock: clock.clone(),
            cloud: http::Api::new(identity, cloud, clock),
            state: Mutex::new(State::default()),
            updating: Mutex::new(()),
        }
    }

    /// Returns the clock of the wire session, which every operation reads.
    pub(crate) fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Ensures cloud sync, reusing the Ark's fresh setup and caching that
    /// decision for 60 s.
    ///
    /// Each caller bounds its own wait, and only one exchange runs at a time.
    pub(crate) fn sync(
        &self,
        requester: &Requester,
        timing: impl Into<Timing>,
    ) -> Result<(), Error> {
        self.ensure(requester, Step::Sync, timing.into())
            .map_err(Into::into)
    }

    /// Refreshes cloud keys and signed time explicitly, dropping the reused
    /// setup.
    ///
    /// A sync already in flight is joined, and a fresh exchange follows when
    /// that sync reused device state.
    pub(crate) fn resync(&self, requester: &Requester, timing: Timing) -> Result<(), Error> {
        self.ensure(requester, Step::Refresh, timing)
            .map_err(Into::into)
    }

    /// Attaches the relay after cloud sync, reusing a healthy attachment.
    ///
    /// A failed relay is replaced on the next call, without replaying any
    /// operation.
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

    /// Establishes one prerequisite, one attempt at a time, while unrelated
    /// device traffic continues.
    ///
    /// Waiters keep the attempt they joined, even if a later caller retries it.
    fn ensure(&self, requester: &Requester, step: Step, timing: Timing) -> Result<(), Failure> {
        let deadline = timing.io(&self.clock);

        // Under the lock, reuse a fresh result, or join or lead the step's attempt
        let (attempt, leader) = {
            let mut state = self.state.lock().expect("cloud setup not poisoned");
            if let Some(error) = &state.error {
                return Err(error.clone().into());
            }
            match step {
                _ if self.cloud.is_none() => return Err(Failure::MissingEnvironment),
                Step::Sync
                    if state.synced.is_some_and(|(at, synced)| {
                        synced && self.clock.elapsed(at) < Duration::from_secs(60)
                    }) =>
                {
                    return Ok(());
                }
                Step::Relay if state.relay.as_ref().is_some_and(relay::Relay::connected) => {
                    return Ok(());
                }
                _ => {}
            }

            // A caller out of time neither starts nor joins an attempt
            if self.clock.now() >= deadline {
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
                    let attempt = Arc::new(Attempt::new(&self.clock));
                    *pending = Some(attempt.clone());
                    (attempt, true)
                }
            }
        };

        // A follower waits for the leader's outcome under its own deadline
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
                        state.synced = Some((self.clock.now(), true));
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

        // Clear the attempt for the next caller and release its waiters
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
                .request(RelayJoinRequest {}, timing.io(&self.clock))?
                .wait::<RelayJoinResponse>()?;
            relay::Relay::connect(
                cloud,
                &cloud.relay_url(),
                &joined.auth,
                requester.clone(),
                timing.io(&self.clock),
            )
        })
    }

    /// Runs an authenticated cloud step, refreshing cloud keys and running it
    /// once more after a refused proof.
    ///
    /// The Ark's sync marker cannot reveal changed cloud keys, so a refused
    /// proof is taken as a sign of stale ones. The step obtains a new proof on
    /// each run. Since it may run twice, callers use it only before any
    /// pairing, approval or transfer begins.
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

    /// Forwards an Ark request to the companion, attaching the relay on demand.
    ///
    /// Replies to wire requests keep arriving while dispatch waits for the
    /// attachment, and a failed attachment refuses the request with
    /// `UNAVAILABLE`. Without a cloud route, the request comes back for the
    /// application's own receive queue.
    pub(crate) fn forward(
        &self,
        requester: &Requester,
        request: RelayArkToAppRequest,
        responder: Responder,
    ) -> Option<(RelayArkToAppRequest, Responder)> {
        if self.cloud.is_none() {
            return Some((request, responder));
        }

        // Attach within the exchange's own time, refusing the request on failure
        let deadline = self.clock.now() + relay::EXCHANGE_TIMEOUT;
        if let Err(error) = self.relay(requester, deadline) {
            relay::fail(responder, &error.to_string());
            return None;
        }

        // Hand the request to the relay, unless it ended meanwhile
        let state = self.state.lock().expect("cloud setup not poisoned");
        match &state.relay {
            Some(relay) => {
                relay.forward(request, responder, deadline);
            }
            None => relay::fail(responder, "relay closed"),
        }
        None
    }

    /// Fetches the Ark's registration with a fresh proof, once cloud sync is
    /// established.
    pub(crate) fn genuine(
        &self,
        requester: &Requester,
        timing: Timing,
    ) -> Result<Registration, Error> {
        let cloud = self.cloud.as_ref().ok_or(Error::MissingEnvironment)?;
        self.sync(requester, timing)?;
        self.authenticate(requester, timing, || {
            let proof = requester
                .request(GenuinityProofRequest {}, timing.io(&self.clock))?
                .wait::<crate::schema::GenuinityProofResponse>()?;
            cloud.genuine(&proof.proof, timing.io(&self.clock))
        })
        .map_err(Into::into)
    }

    /// Exchanges cloud keys and signed time with the Ark, unless `force` is off
    /// and its setup is fresh.
    ///
    /// The exchange uses raw wire requests, bypassing the prerequisite gate that
    /// it completes. Returns whether an exchange ran.
    fn synchronize(
        &self,
        requester: &Requester,
        timing: Timing,
        force: bool,
    ) -> Result<bool, Failure> {
        // Reuse a recent observation of the Ark's setup, or ask the Ark
        if !force {
            let reported = self
                .state
                .lock()
                .expect("cloud setup not poisoned")
                .synced
                .filter(|&(at, _)| self.clock.elapsed(at) < Duration::from_secs(60))
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

        // Pass the cloud's certificates to the Ark, then signed time for its
        // challenge
        let cloud = self.cloud.as_ref().expect("cloud route available");
        let identity = cloud.with_auth(timing, || cloud.identity(timing.io(&self.clock)))?;
        let started = requester
            .request(identity, timing.io(&self.clock))?
            .wait::<crate::schema::CloudSyncStartResponse>()?;
        let time = cloud.with_auth(timing, || {
            cloud.time(&started.challenge, timing.io(&self.clock))
        })?;
        requester
            .request(time, timing.io(&self.clock))?
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
            .request(crate::schema::DeviceInfoRequest {}, timing.io(&self.clock))?
            .wait::<crate::schema::DeviceInfoResponse>()?;
        Ok(cloud_synced(&info, &self.clock))
    }

    /// Records the cloud setup that a caller's own device info request
    /// observed.
    ///
    /// A delayed response never overwrites a newer observation or a sync
    /// exchange in flight.
    pub(crate) fn reported(&self, info: &crate::schema::DeviceInfoResponse, requested: Instant) {
        let mut state = self.state.lock().expect("cloud setup not poisoned");
        if state.error.is_none()
            && state.syncing.is_none()
            && state.synced.is_none_or(|(at, _)| at < requested)
        {
            state.synced = Some((requested, cloud_synced(info, &self.clock)));
        }
    }

    /// Ends setup and relay traffic when the owning connection closes.
    pub(crate) fn close(&self) {
        self.end(protocol::Error::Closed);
    }

    /// Ends setup with the wire's ending reason, releasing waiters and closing
    /// the relay.
    ///
    /// Only the first reason is kept, and later calls do nothing.
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

/// Setup step that callers join or start.
///
/// Relay callers establish cloud sync first.
#[derive(Clone, Copy)]
enum Step {
    /// Cloud sync, reusing fresh device state when it can.
    Sync,
    /// Cloud sync that exchanges keys and time even when the device reports
    /// usable setup.
    Refresh,
    /// Relay attachment with its own worker, reusing a healthy one.
    Relay,
}

impl Attempt {
    /// Creates an attempt whose waiters measure their deadlines on the clock.
    fn new(clock: &Clock) -> Self {
        Self {
            result: sync::Mutex::new(None),
            refreshed: AtomicBool::new(false),
            ready: sync::Condvar::new(clock),
        }
    }

    /// Publishes the first outcome, preserving closure if it won the race.
    fn finish(&self, result: Result<(), Failure>) {
        let mut outcome = self.result.lock().expect("cloud attempt not poisoned");
        if outcome.is_none() {
            *outcome = Some(result);
            self.ready.notify_all();
        }
    }

    /// Waits for this attempt without extending or shortening another caller's
    /// budget.
    fn wait(&self, deadline: Instant) -> Result<(), Failure> {
        let mut outcome = self.result.lock().expect("cloud attempt not poisoned");
        loop {
            if let Some(result) = &*outcome {
                return result.clone();
            }
            let (next, wait) = self
                .ready
                .wait_deadline(outcome, deadline)
                .expect("cloud attempt not poisoned");
            outcome = next;
            if wait.timed_out() && outcome.is_none() {
                return Err(protocol::Error::Timeout.into());
            }
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
    use crate::testing::{Peer, answering, test_clock, wait_deadline};
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

    /// Serves scripted responses in order and captures each request's headers.
    ///
    /// Each connection closes after its response. A server left waiting by a
    /// failed test blocks only its own thread.
    pub(crate) fn serve(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
        serve_inner(responses, None)
    }

    /// Serves like [`serve`], holding the first response until `pause` releases
    /// it.
    ///
    /// Holding a response makes setup races deterministic.
    pub(super) fn serve_inner(
        responses: Vec<String>,
        mut pause: Option<mpsc::Receiver<()>>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(TIMEOUT)).unwrap();

                // Capture the request up to the end of its headers
                let mut request = Vec::new();
                let mut bytes = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    match stream.read(&mut bytes) {
                        Ok(0) | Err(_) => return,
                        Ok(count) => request.extend_from_slice(&bytes[..count]),
                    }
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();

                // Hold the first response until released, then answer
                if let Some(pause) = pause.take()
                    && pause.recv().is_err()
                {
                    return;
                }
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

    /// Builds the HTTP client of the loopback tests, configured like the cloud's
    /// own but ignoring ambient proxy settings.
    pub(super) fn http() -> ureq::Agent {
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into()
    }

    /// Attaches a real wire session whose cloud routes lead to the test server.
    ///
    /// The connection runs on the clock of the peer's stream.
    pub(crate) fn attach(peer: &mut Peer, url: String) -> Ark {
        let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let clock = session.clock();
        let services = Arc::new(Services {
            clock: clock.clone(),
            cloud: Some(super::http::tests::api(url, Realm::Hardware, &clock)),
            state: Mutex::new(State::default()),
            updating: Mutex::new(()),
        });
        Ark::start(session, services).unwrap()
    }

    /// Spawns an Ark peer that issues proofs and lists slots only after sync,
    /// counting its sync starts and proofs.
    ///
    /// With `refuse_first` set, it refuses its first sync start.
    fn peer(clock: &Clock, refuse_first: bool) -> (Peer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let proofs = Arc::new(AtomicUsize::new(0));
        let peer = Peer::spawn(
            clock,
            Box::new({
                let starts = starts.clone();
                let proofs = proofs.clone();
                let mut synced = false;
                move |session, request, responder| {
                    let deadline = session.clock().now() + TIMEOUT;
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
            }),
        );
        (peer, starts, proofs)
    }

    /// Returns the cloud's identity and time replies for one sync exchange.
    pub(super) fn sync_responses() -> Vec<String> {
        vec![
            response(200, r#"{"signer":"AQ==","crypto":"Ag=="}"#),
            response(200, r#"{"unixmilli":123,"signature":"BA=="}"#),
        ]
    }

    /// Concurrent clients share setup, status bypasses it, short waiters expire
    /// independently and subsequent operations reuse the successful exchange.
    #[test]
    fn test_shared_setup() {
        // Hold the leader's sync at its first HTTP request
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut responses = sync_responses();
        responses.push(response(200, r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#));
        let (release, pause) = mpsc::channel();
        let (url, requests) = serve_inner(responses, Some(pause));
        let (mut peer, starts, proofs) = peer(&clock, false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        requests.recv().unwrap();

        // Status bypasses the pending setup
        assert_eq!(
            client
                .call(DeviceInfoRequest {}, deadline)
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);

        // A short waiter on the pending setup expires alone, at the earliest
        // deadline on the clock
        let short = clock.now() + Duration::from_millis(20);
        let waiter = thread::spawn({
            let client = client.clone();
            move || client.call_timeout(GenuinityProofRequest {}, Duration::from_millis(20))
        });
        wait_deadline(&tester, short);
        tester.advance_to(short);
        assert!(matches!(waiter.join().unwrap(), Err(Error::Timeout)));

        // A later waiter and the leader both finish on the one exchange
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
                .recv()
                .unwrap()
                .starts_with("GET /v1/cloudsync/time?challenge=03 ")
        );
        assert!(requests.recv().unwrap().starts_with("GET /v1/genuine "));
    }

    /// A refused attempt preserves its device error and does not poison a retry.
    #[test]
    fn test_setup_retry() {
        // The Ark refuses the first sync start, whose error comes back unchanged
        let clock = test_clock().clock();
        let mut responses = sync_responses();
        responses.insert(0, responses[0].clone());
        let (url, _requests) = serve(responses);
        let (mut peer, starts, proofs) = peer(&clock, true);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        assert!(matches!(client.call(GenuinityProofRequest {}, deadline),
            Err(Error::Remote(error)) if error.code == 0x111));

        // A retry syncs afresh and gets its proof
        client.call(GenuinityProofRequest {}, deadline).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(proofs.load(Ordering::SeqCst), 1);
    }

    /// Closing releases setup waiters before the outstanding HTTP response arrives
    /// and prevents that response from starting any request on the closed Ark.
    #[test]
    fn test_close_during_setup() {
        // Hold the leader's sync at its first HTTP request
        let clock = test_clock().clock();
        let (release, pause) = mpsc::channel();
        let (url, requests) = serve_inner(vec![sync_responses().remove(0)], Some(pause));
        let (mut peer, starts, _) = peer(&clock, false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        requests.recv().unwrap();

        // Closing releases a waiter on the held sync at once
        let waiter = thread::spawn({
            let client = client.clone();
            move || client.call(GenuinityProofRequest {}, deadline)
        });
        ark.closer().close();
        assert!(matches!(waiter.join().unwrap(), Err(Error::Closed)));

        // The leader fails once its response arrives, starting no Ark request
        release.send(()).unwrap();
        assert!(matches!(leader.join().unwrap(), Err(Error::Closed)));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    /// Self-signed and recovery sessions without an environment fail cloud
    /// requests clearly and keep local ones.
    #[test]
    fn test_unattested_setup() {
        let clock = test_clock().clock();
        for recover in [false, true] {
            // Attach without attestation or an environment
            let mut peer = Peer::spawn(&clock, Box::new(answering));
            let policy = if recover {
                TrustMode::Recover(Box::new(peer.identity.clone()))
            } else {
                TrustMode::RootOrSelf
            };
            let (ark, _) = Ark::attach(peer.stream(), &policy, |_| None).unwrap();

            // Cloud requests fail for want of an environment, while status works
            let client = ark.client();
            assert!(matches!(
                client.call(GenuinityProofRequest {}, clock.now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            assert!(matches!(
                client.genuine(clock.now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            assert!(matches!(
                client.call(crate::schema::UnlockRequest {}, clock.now() + TIMEOUT),
                Err(Error::MissingEnvironment)
            ));
            client
                .call(DeviceInfoRequest {}, clock.now() + TIMEOUT)
                .unwrap();
        }
    }

    /// An explicit route lets self-signed and recovery peers sync, query slots
    /// and pass registry checks with no attested serial.
    #[test]
    fn test_unattested_cloud() {
        let clock = test_clock().clock();
        for recover in [false, true] {
            for realm in [Realm::Hardware, Realm::Emulator] {
                // Serve one sync and registration, then two refusals around a
                // second sync
                let mut responses = sync_responses();
                responses.push(response(200, r#"{"serial":"registry-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#));
                responses.push(response(403, "registry refused proof"));
                responses.extend(sync_responses());
                responses.push(response(403, "registry refused proof"));
                let (url, requests) = serve(responses);

                // Connect without attestation, which leaves the realm unset
                let (mut peer, starts, proofs) = peer(&clock, false);
                let policy = if recover {
                    TrustMode::Recover(Box::new(peer.identity.clone()))
                } else {
                    TrustMode::RootOrSelf
                };
                let (session, identity) = protocol::connect(peer.stream(), &policy).unwrap();
                assert_eq!(identity.realm(), None);
                assert_eq!(matches!(identity, Identity::Recovered(_)), recover);

                // Route the cloud explicitly, then point it at the loopback server
                let env = crate::identity::ENVIRONMENTS[0];
                let mut services = Services::new(&identity, Some((env, realm)), &session.clock());
                let cloud = services.cloud.as_mut().unwrap();
                cloud.url = url;
                cloud.agent = http();
                let ark = Ark::start(session, Arc::new(services)).unwrap();
                let client = ark.client();
                let deadline = clock.now() + TIMEOUT;

                // Status needs no sync, and two slot queries share one
                client.call(DeviceInfoRequest {}, deadline).unwrap();
                assert_eq!(starts.load(Ordering::SeqCst), 0);
                for _ in 0..2 {
                    client
                        .call(crate::schema::SlotListRequest {}, deadline)
                        .unwrap();
                }

                // The registry answers with no attested serial to match, and a
                // later refusal persists through one resync
                let registration = client.genuine(deadline).unwrap();
                assert_eq!(registration.serial, "registry-serial");
                assert!(registration.active());
                assert!(matches!(
                    client.genuine(deadline),
                    Err(Error::ProofRejected)
                ));
                assert_eq!(starts.load(Ordering::SeqCst), 2);
                assert_eq!(proofs.load(Ordering::SeqCst), 3);

                // Every request took the realm's routes, in order
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
                    let request = requests.recv().unwrap();
                    assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                }
            }
        }
    }

    /// Device setup counts as fresh only with the boot marker set and a clock
    /// less than 15 s away from the host's.
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

    /// A refused proof triggers one resync and a new proof, while other HTTP
    /// failures and a second refusal keep their errors.
    #[test]
    fn test_authentication_refresh() {
        use crate::schema;
        let clock = test_clock().clock();
        for operation in ["genuine", "relaying", "pairing"] {
            for (status, retried) in [(400, 400), (403, 403), (403, 200), (503, 503)] {
                if retried == 200 && operation != "genuine" {
                    continue;
                }

                // Answer the first attempt with the status, and the retry after
                // a 403 with the retry status
                let mut responses = vec![response(status, "refused")];
                if status == 403 {
                    responses.extend(sync_responses());
                    responses.push(response(retried, if retried == 200 {
                        r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#
                    } else { "still refused" }));
                }
                let (url, requests) = serve(responses);

                // An Ark reporting fresh setup, whose proofs change once it resyncs
                let proofs = Arc::new(AtomicUsize::new(0));
                let mut peer = Peer::spawn(
                    &clock,
                    Box::new({
                        let proofs = proofs.clone();
                        let mut refreshed = false;
                        move |session, request, responder| {
                            let deadline = session.clock().now() + TIMEOUT;
                            let proof = vec![u8::from(refreshed)];
                            let response: protocol::Message = match request {
                                Content::DeviceInfo(_) => schema::DeviceInfoResponse {
                                    cloud_synced: true,
                                    cloud_clock: session
                                        .clock()
                                        .system_time()
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
                    }),
                );

                // Run the operation, which never starts pairing before it
                // authenticates
                let ark = attach(&mut peer, url);
                let client = ark.client();
                let deadline = clock.now() + TIMEOUT;
                let result = match operation {
                    "genuine" => client.genuine(deadline).map(drop),
                    "relaying" => client.attach_relay(deadline),
                    "pairing" => {
                        client.pair(deadline, |_| panic!("pairing began before authentication"))
                    }
                    _ => unreachable!(),
                };

                // A successful retry passes, a second 403 stays a refused proof,
                // and other statuses stay cloud failures
                if retried == 200 {
                    result.unwrap();
                } else if status == 403 {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::ProofRejected), "{error:?}");
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, Error::Cloud(_)), "{error:?}");
                }

                // A 403 resyncs between two attempts, each with its own proof
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

    /// Refused credentials trigger one login and a retry with a fresh proof and
    /// no resync, and a failed login or a second refusal ends the operation.
    #[test]
    fn caller_authentication_retries_fresh_proofs_without_cloud_sync() {
        use auth::tests::{Login, refused};
        let clock = test_clock().clock();
        for operation in ["genuine", "relaying", "pairing"] {
            for fail_login in [false, true] {
                // Refuse the caller's credentials, and after a login accept only
                // the registry check
                let mut responses = vec![refused(403)];
                if !fail_login {
                    responses.push(if operation == "genuine" {
                        response(200, r#"{"serial":"test-serial","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#)
                    } else { refused(403) });
                }
                let (url, requests) = serve(responses);

                // An Ark reporting fresh setup, numbering each proof it issues
                let proofs = Arc::new(AtomicUsize::new(0));
                let mut peer = Peer::spawn(
                    &clock,
                    Box::new({
                        let proofs = proofs.clone();
                        move |session, request, responder| {
                            let response: protocol::Message = match request {
                                Content::DeviceInfo(_) => crate::schema::DeviceInfoResponse {
                                    cloud_synced: true,
                                    cloud_clock: session
                                        .clock()
                                        .system_time()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs(),
                                    ..Default::default()
                                }
                                .into(),
                                Content::GenuinityProof(_) => {
                                    crate::schema::GenuinityProofResponse {
                                        proof: vec![proofs.fetch_add(1, Ordering::SeqCst) as u8],
                                    }
                                    .into()
                                }
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
                            responder
                                .reply(response, session.clock().now() + TIMEOUT)
                                .unwrap();
                            true
                        }
                    }),
                );

                // Status never consults the login stand-in
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

                // Only a registry check after a successful login passes
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

                // Every case logs in once, and a retry carries refreshed
                // credentials and a fresh proof, with no resync in between
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

    /// Cloud sync logs in when the identity request is refused, before any
    /// certificate reaches the Ark.
    #[test]
    fn cloud_sync_logs_in_before_sending_certificates_to_the_ark() {
        use auth::tests::{Login, refused};

        // Refuse the caller's credentials on the first identity request
        let mut responses = vec![refused(302)];
        responses.extend(sync_responses());
        let (url, requests) = serve(responses);

        // An explicit sync logs in once and hands the Ark a single identity
        let (mut peer, starts, _) = peer(&test_clock().clock(), false);
        let mut ark = attach(&mut peer, url);
        let login = Login::default();
        ark.set_cloud_auth(login.clone());
        ark.client().sync(Timing::inactivity(TIMEOUT)).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(login.logins.load(Ordering::SeqCst), 1);

        // The refused request carried the cached credentials, and the two after
        // the login carried refreshed ones
        let requests: Vec<_> = requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("authorization: cached\r\n"));
        assert!(requests[1].contains("authorization: refreshed\r\n"));
        assert!(requests[2].contains("authorization: refreshed\r\n"));
    }

    /// Reported device setup spares the sync exchange, an explicit sync always
    /// runs it, and dataset paths keep every field across the connection.
    #[test]
    fn test_reported_sync_and_explicit_refresh() {
        use crate::schema;

        // A dataset path with every field set
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
        let clock = test_clock().clock();
        for initially_synced in [false, true] {
            // Serve the explicit sync, plus a first one for an unsynced Ark
            let mut responses = sync_responses();
            if !initially_synced {
                responses.extend(sync_responses());
            }
            let (url, requests) = serve(responses);

            // An Ark counting status requests, serving paths only once synced
            let infos = Arc::new(AtomicUsize::new(0));
            let mut peer = Peer::spawn(
                &clock,
                Box::new({
                    let infos = infos.clone();
                    let paths = expected.clone();
                    let mut synced = initially_synced;
                    move |session, request, responder| {
                        let deadline = session.clock().now() + TIMEOUT;
                        let response: protocol::Message = match request {
                            Content::DeviceInfo(_) => {
                                infos.fetch_add(1, Ordering::SeqCst);
                                let clock = session
                                    .clock()
                                    .system_time()
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
                }),
            );

            // Requests after a status reuse what it reported, syncing only when
            // the Ark was not synced
            let ark = attach(&mut peer, url);
            let client = ark.client();
            let deadline = clock.now() + TIMEOUT;
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

            // An explicit sync always runs the exchange, without asking the Ark
            client.sync(deadline).unwrap();
            assert!(requests.recv().unwrap().contains("/cloudsync/identity"));
            assert!(requests.recv().unwrap().contains("/cloudsync/time"));
            assert_eq!(infos.load(Ordering::SeqCst), 1);
        }
    }

    /// An older status response read after a refresh does not undo it.
    #[test]
    fn test_delayed_device_info_retains_newer_sync() {
        // Read a status sent before an explicit sync only after the sync
        let clock = test_clock().clock();
        let (url, requests) = serve(sync_responses());
        let (mut peer, starts, _) = peer(&clock, false);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        let pending = client.send(DeviceInfoRequest {}, deadline).unwrap();
        client.sync(deadline).unwrap();
        assert!(!pending.wait().unwrap().cloud_synced);

        // A later request reuses the sync instead of running another
        client
            .call(crate::schema::SlotListRequest {}, deadline)
            .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(requests.try_iter().count(), 2);
    }

    /// Only an `UNAVAILABLE` refusal with lost sync reported triggers a resync
    /// and one retry, and every other refusal comes back unchanged.
    #[test]
    fn test_unavailable_retry_requires_lost_sync() {
        use crate::schema::{self, ReservedErrors};
        let clock = test_clock().clock();

        // Each case names the refusal, whether it drops the Ark's sync, whether
        // it repeats and the retries expected
        for (code, reset, repeat, retries) in [
            (ReservedErrors::Unavailable as u64, true, false, 1),
            (ReservedErrors::Unavailable as u64, true, true, 1),
            (ReservedErrors::Unavailable as u64, false, false, 0),
            (ReservedErrors::Unauthorized as u64, true, false, 0),
            (0x1234, true, false, 0),
        ] {
            // Serve one sync exchange when the case retries
            let (url, requests) = serve(if retries == 1 {
                sync_responses()
            } else {
                vec![]
            });

            // An Ark that refuses slot listings as the case asks, reporting its
            // sync lost on reset
            let count = Arc::new(AtomicUsize::new(0));
            let mut peer = Peer::spawn(
                &clock,
                Box::new({
                    let count = count.clone();
                    let mut synced = true;
                    move |session, request, responder| {
                        let deadline = session.clock().now() + TIMEOUT;
                        let response: protocol::Message = match request {
                            Content::DeviceInfo(_) => {
                                let clock = session
                                    .clock()
                                    .system_time()
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
                                        .fail(
                                            schema::Error::new(code, "original refusal"),
                                            deadline,
                                        )
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
                }),
            );

            // The listing retries only as expected, ending in success or the
            // original refusal
            let ark = attach(&mut peer, url);
            let result = ark
                .client()
                .call(schema::SlotListRequest {}, clock.now() + TIMEOUT);
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
