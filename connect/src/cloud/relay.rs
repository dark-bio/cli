// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Carries opaque companion messages between the cloud socket and the Ark.
//!
//! One worker owns the WebSocket and both directions of exchange bookkeeping.
//! The wire dispatcher only admits reverse requests to a bounded queue. Relay
//! failure refuses retained Ark requests; a later operation may attach again,
//! but no request is replayed and the underlying wire session stays independent.
//!
//! The worker waits on the socket through mio, so its heartbeat and write
//! timers run on real time. Exchange deadlines belong to the wire session and
//! are measured on its clock.

use super::{
    Failure,
    socket::{self, Socket, io_error, socket_error, socket_mut},
};
use crate::timing::ClockExt;
use darkbio_crypto::cbor::{self, Cbor};
use darkbio_wire::protocol::{self, Promise, Requester, Responder, schema};
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Relay bodies fit inside a wire message. Queues and requests also have limits.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;
/// Byte allowance for each queued direction, excluding in-flight wire messages.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Request count limit applied to admission, active exchanges and output queues.
const MAX_INFLIGHT: usize = 128;

/// The firmware bounds its relay requests by sixty seconds. This also bounds
/// retained responders when a companion never answers, independently of callers.
pub(super) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time to flush an outgoing frame through a backlogged cloud socket.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle interval between a valid pong and the next liveness probe.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Maximum wait for the pong matching the current probe.
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// Readiness token for incoming traffic and pending socket writes.
const SOCKET: Token = Token(0);
/// Readiness token for queued Ark requests or local closure.
const WAKE: Token = Token(1);

/// An attached relay. Dropping it ends its worker and outstanding forwarding.
#[derive(Debug)]
pub(super) struct Relay {
    shared: Arc<Shared>,    // Queue and ending reason observed by the dispatcher
    worker: Option<Worker>, // Started when dispatch can see this attachment
}

/// Connected resources moved into the worker only after services publishes the relay.
#[derive(Debug)]
struct Worker {
    /// Sole owner of WebSocket framing and TLS state.
    socket: WebSocket<MaybeTlsStream<Socket>>,
    /// Waits for socket readiness, admission wakeups and exchange deadlines.
    poll: Poll,
    /// Issues companion requests through the original Ark session.
    requester: Requester,
    /// Cloud liveness probes, independent of companion availability.
    heartbeat: Heartbeat,
}

/// Admission and shutdown handles shared by the dispatcher and relay worker.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>, // Admission and closure are atomic with respect to each other
    wake: Arc<Waker>,    // signals queued Ark traffic, wire completions or local closure
    socket: TcpStream,   // Interrupts reads even inside WebSocket message assembly
    /// Notifies a test once the last handle to this relay is gone.
    #[cfg(test)]
    dropped: Mutex<Option<mpsc::Sender<()>>>,
}

/// Reverse requests awaiting admission and the first attachment failure.
#[derive(Debug, Default)]
struct State {
    /// Ark requests, unanswered responders and their fixed exchange deadlines.
    queue: VecDeque<(schema::RelayArkToAppRequest, Responder, Instant)>,
    bytes: usize,          // Opaque bytes waiting for the socket worker
    error: Option<String>, // First reason this relay ended
}

impl Relay {
    /// Opens an authenticated socket under the original operation's deadline.
    pub(super) fn connect(
        api: &super::http::Api,
        url: &str,
        auth: &[u8],
        requester: Requester,
        deadline: Instant,
    ) -> Result<Self, Failure> {
        let mut socket = socket::connect(api, url, auth, "Relaying", deadline)?;
        api.clock.remaining(deadline).map_err(io_error)?;
        let Socket::Blocking { stream, .. } = socket_mut(&mut socket) else {
            unreachable!()
        };
        stream.set_read_timeout(None).map_err(io_error)?;
        stream.set_write_timeout(None).map_err(io_error)?;
        stream.set_nonblocking(true).map_err(io_error)?;
        let shutdown = stream.try_clone().map_err(io_error)?;
        let mut connected = mio::net::TcpStream::from_std(stream.try_clone().map_err(io_error)?);
        let poll = Poll::new().map_err(io_error)?;
        poll.registry()
            .register(
                &mut connected,
                SOCKET,
                Interest::READABLE | Interest::WRITABLE,
            )
            .map_err(io_error)?;
        *socket_mut(&mut socket) = Socket::Connected(connected);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Arc::new(Waker::new(poll.registry(), WAKE).map_err(io_error)?),
            socket: shutdown,
            #[cfg(test)]
            dropped: Mutex::new(None),
        });
        Ok(Self {
            shared,
            worker: Some(Worker {
                socket,
                poll,
                requester,
                heartbeat: Heartbeat::attach(),
            }),
        })
    }

    /// Starts under the services lock so a companion request arriving immediately
    /// after upgrade cannot provoke Ark traffic before dispatch sees this relay.
    pub(super) fn start(&mut self) -> Result<(), Failure> {
        let Worker {
            socket,
            poll,
            requester,
            heartbeat,
        } = self.worker.take().expect("relay worker started once");
        thread::Builder::new()
            .name("ark-relay".into())
            .spawn({
                let shared = self.shared.clone();
                move || pump(socket, poll, requester, shared, heartbeat)
            })
            .map_err(io_error)?;
        Ok(())
    }

    /// Whether this attachment has no recorded failure. This is a local snapshot;
    /// a dead peer may remain undetected until I/O or the next heartbeat expires.
    pub(super) fn connected(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("relay queue not poisoned")
            .error
            .is_none()
    }

    /// Queues a reverse request without blocking the Ark's receive loop.
    pub(super) fn forward(
        &self,
        request: schema::RelayArkToAppRequest,
        responder: Responder,
        deadline: Instant,
    ) {
        let mut state = self.shared.state.lock().expect("relay queue not poisoned");
        if let Some(error) = &state.error {
            fail(responder, error);
        } else if state.queue.len() >= MAX_INFLIGHT
            || request.req.len() > MAX_BYTES.saturating_sub(state.bytes)
            || request.req.len() > MAX_MESSAGE - 64
        {
            fail(responder, "relay request queue full or message too large");
        } else {
            state.bytes += request.req.len();
            state.queue.push_back((request, responder, deadline));
            let _ = self.shared.wake.wake();
        }
    }

    /// Refuses queued work and wakes the worker without waiting for it to join.
    pub(super) fn close(&self) {
        self.shared.end("relay closed".into());
    }
}

impl Drop for Relay {
    /// Ends this attachment even if the owning wire connection remains open.
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    /// Refuses queued work and interrupts the socket, retaining the first failure.
    fn end(&self, error: String) {
        let mut state = self.state.lock().expect("relay queue not poisoned");
        if state.error.is_none() {
            state.error = Some(error);
            let _ = self.socket.shutdown(Shutdown::Both);
            let queued = std::mem::take(&mut state.queue);
            state.bytes = 0;
            for (_, responder, _) in queued {
                fail(responder, state.error.as_ref().unwrap());
            }
            let _ = self.wake.wake();
        }
    }
}

#[cfg(test)]
impl Drop for Shared {
    /// Notifies the test that the relay and its worker released their handles.
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.get_mut().unwrap().take() {
            let _ = sender.send(());
        }
    }
}

/// Recognizes an incomplete nonblocking operation that the poll loop can resume.
fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

/// An unavailable relay fails the Ark's reverse request, allowing its original
/// operation to finish with an error. Enqueueing the reply does not wait on I/O.
pub(super) fn fail(responder: Responder, reason: &str) {
    let error = schema::Error::reserved(schema::ReservedErrors::Unavailable, reason);
    let deadline = responder.clock().now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT;
    let _ = responder.fail(error, deadline);
}

/// Current cloud envelope. Only the envelope is interpreted; all bodies stay sealed.
#[derive(Cbor, Default)]
#[cbor(array)]
struct Envelope {
    /// Envelope version; the current cloud protocol requires one.
    darkrpc: u64,
    /// Correlation ID present only on requests and responses.
    id: Option<u64>,
    /// Sealed notification body, currently ignored after envelope validation.
    notify: Option<Vec<u8>>,
    /// Sealed request body forwarded without interpretation.
    request: Option<Vec<u8>>,
    /// Sealed response body matched to an outstanding request.
    response: Option<Vec<u8>>,
    /// Presence body, currently ignored because wire has no corresponding input.
    presence: Option<Vec<u8>>,
}

/// Validated envelope shape. The host only originates requests and responses.
enum Frame {
    /// Correlation ID and opaque request bytes.
    Request(u64, Vec<u8>),
    /// Correlation ID and opaque response bytes.
    Response(u64, Vec<u8>),
    /// Recognized notification or presence envelope with no wire destination.
    Notice,
}

impl Frame {
    /// Requires the current version and exactly one payload with the proper ID shape.
    fn decode(bytes: &[u8]) -> Result<Self, Failure> {
        let envelope: Envelope = cbor::decode(bytes)
            .map_err(|error| Failure::Relay(format!("invalid relay envelope: {error}")))?;
        if envelope.darkrpc != 1 {
            return Err(Failure::Relay("unsupported relay envelope version".into()));
        }
        match (
            envelope.id,
            envelope.request,
            envelope.response,
            envelope.notify,
            envelope.presence,
        ) {
            (Some(id), Some(req), None, None, None) => Ok(Self::Request(id, req)),
            (Some(id), None, Some(res), None, None) => Ok(Self::Response(id, res)),
            (None, None, None, Some(_), None) | (None, None, None, None, Some(_)) => {
                Ok(Self::Notice)
            }
            _ => Err(Failure::Relay("invalid relay envelope shape".into())),
        }
    }

    /// Encodes a forwarded exchange without changing its ID or sealed payload.
    /// Notifications cannot be originated by the host.
    fn encode(self) -> Vec<u8> {
        let mut envelope = Envelope {
            darkrpc: 1,
            ..Default::default()
        };
        match self {
            Self::Request(id, req) => {
                envelope.id = Some(id);
                envelope.request = Some(req);
            }
            Self::Response(id, res) => {
                envelope.id = Some(id);
                envelope.response = Some(res);
            }
            Self::Notice => unreachable!("the host originates no notifications"),
        }
        cbor::encode(envelope).expect("relay envelope contains only integers and bytes")
    }
}

/// Checks the cloud transport independently of companion availability. Only a
/// pong echoing our current ping proves liveness; other traffic cannot defer it.
#[derive(Debug)]
struct Heartbeat {
    /// Delay before probing again after a matching pong.
    interval: Duration,
    /// Time allowed for the next probe's matching pong.
    timeout: Duration,
    /// Next probe time when no ping is awaiting a pong.
    next: Instant,
    /// Wrapping probe ID, encoded in network byte order.
    sequence: u64,
    /// Probe ID and fixed expiry while awaiting a matching pong.
    pending: Option<(u64, Instant)>,
}

impl Heartbeat {
    /// Schedules the first probe of a relay attaching now. The worker's socket
    /// timers run on real time, so the schedule starts on it too.
    #[expect(
        clippy::disallowed_methods,
        reason = "the heartbeat is one of the worker's socket timers, which run on real time from attachment"
    )]
    fn attach() -> Self {
        Self::new(PING_INTERVAL, PONG_TIMEOUT, Instant::now())
    }

    /// Schedules the first probe an interval after `now`, sending no traffic
    /// during attachment.
    fn new(interval: Duration, timeout: Duration, now: Instant) -> Self {
        Self {
            interval,
            timeout,
            next: now + interval,
            sequence: 0,
            pending: None,
        }
    }

    /// Produces at most one ping until its matching pong arrives or expires.
    fn ping(&mut self, now: Instant) -> Result<Option<[u8; 8]>, Failure> {
        if let Some((_, deadline)) = self.pending {
            if now >= deadline {
                return Err(Failure::Relay("cloud relay heartbeat timed out".into()));
            }
        } else if now >= self.next {
            self.sequence = self.sequence.wrapping_add(1);
            self.pending = Some((self.sequence, now + self.timeout));
            return Ok(Some(self.sequence.to_be_bytes()));
        }
        Ok(None)
    }

    /// Acknowledges only a timely pong echoing the outstanding probe exactly.
    fn pong(&mut self, bytes: &[u8], now: Instant) {
        if let Some((sequence, deadline)) = self.pending
            && now < deadline
            && bytes == sequence.to_be_bytes()
        {
            self.pending = None;
            self.next = now + self.interval;
        }
    }

    /// Next instant the worker must wake to send a probe or detect its expiry.
    fn deadline(&self) -> Instant {
        self.pending.map_or(self.next, |(_, deadline)| deadline)
    }
}

/// Bounds output the cloud socket has not taken yet. The first write or
/// deferred flush starts the bound, later ones keep its deadline, and only a
/// completed flush ends it.
#[derive(Debug, Default)]
struct Backlog {
    /// Time the pending output must be flushed by, while there is some.
    deadline: Option<Instant>,
}

impl Backlog {
    /// Starts the bound at `now` for output left to flush, keeping the
    /// deadline of a bound already running.
    fn start(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + WRITE_TIMEOUT);
    }

    /// Ends the bound once a flush took all output.
    fn flushed(&mut self) {
        self.deadline = None;
    }

    /// Whether output is left to flush, which holds back the next frame.
    fn active(&self) -> bool {
        self.deadline.is_some()
    }

    /// Whether the output left to flush missed its deadline by `now`.
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Time left after `now` before the deadline, bounding the next poll.
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }
}

/// Writes one frame at a time while receiving concurrently. Wire completions
/// and queued Ark requests wake the poll; idle wakeups check liveness. The
/// socket's own timers read real time and exchange deadlines the session's clock.
#[expect(
    clippy::disallowed_methods,
    reason = "the worker waits on the cloud socket through mio, so its heartbeat and write timers run on real time"
)]
fn pump(
    mut socket: WebSocket<MaybeTlsStream<Socket>>,
    mut poll: Poll,
    requester: Requester,
    shared: Arc<Shared>,
    mut heartbeat: Heartbeat,
) {
    let clock = requester.clock();
    let mut events = Events::with_capacity(8);
    // The two directions have independent ID spaces. Only the worker changes
    // these maps, so completions never need the shared admission lock.
    let mut pending: HashMap<u64, (Responder, Instant)> = HashMap::new();
    let mut inbound: HashMap<u64, Promise<protocol::Message>> = HashMap::new();
    let (answered, answers) = mpsc::channel();
    let mut output = VecDeque::new();
    let mut bytes = 0usize;
    let mut writing = Backlog::default();
    let result = (|| -> Result<(), Failure> {
        loop {
            {
                let mut state = shared.state.lock().expect("relay queue not poisoned");
                if let Some(error) = &state.error {
                    return Err(Failure::Relay(error.clone()));
                }
                // Admission stops while the socket is backlogged; reading and
                // completion handling continue independently of that backlog.
                if output.is_empty()
                    && !writing.active()
                    && pending.len() < MAX_INFLIGHT
                    && let Some((request, responder, deadline)) = state.queue.pop_front()
                {
                    state.bytes -= request.req.len();
                    if deadline <= clock.now() {
                        fail(responder, "relay request timed out in queue");
                    } else if let std::collections::hash_map::Entry::Vacant(entry) =
                        pending.entry(request.id)
                    {
                        entry.insert((responder, deadline));
                        enqueue(
                            &mut output,
                            &mut bytes,
                            Frame::Request(request.id, request.req),
                        )?;
                    } else {
                        fail(responder, "duplicate relay request id");
                    }
                }
            }
            // Expired exchanges release their responder even if no further
            // traffic arrives. Responses without a pending responder are discarded.
            let now = clock.now();
            let expired: Vec<_> = pending
                .iter()
                .filter(|(_, (_, deadline))| now >= *deadline)
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                fail(pending.remove(&id).unwrap().0, "relay request timed out");
            }
            while let Ok(id) = answers.try_recv() {
                if let Some(promise) = inbound.remove(&id) {
                    let response = match promise.wait::<schema::RelayArkToAppResponse>() {
                        Ok(response) => response,
                        // Only the Ark can seal an error for the companion. Let
                        // this exchange expire there without ending unrelated ones.
                        Err(
                            protocol::Error::Remote(_)
                            | protocol::Error::Timeout
                            | protocol::Error::TooLarge(_),
                        ) => continue,
                        Err(error) => return Err(error.into()),
                    };
                    if response.id != id {
                        return Err(Failure::Relay(
                            "Ark answered another relay request id".into(),
                        ));
                    }
                    enqueue(&mut output, &mut bytes, Frame::Response(id, response.res))?;
                }
            }
            if !writing.active()
                && let Some(frame) = output.pop_front()
            {
                bytes -= frame.len();
                writing.start(Instant::now());
                match socket.write(Message::Binary(frame.into())) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(socket_error(error)),
                }
            }
            // Bound each read batch so a busy companion cannot starve writes,
            // expired Ark requests or the heartbeat.
            let mut batch_full = false;
            for index in 0..32 {
                match socket.read() {
                    Ok(Message::Binary(bytes)) => match Frame::decode(&bytes)? {
                        Frame::Request(id, req) => {
                            if inbound.len() >= MAX_INFLIGHT || inbound.contains_key(&id) {
                                return Err(Failure::Relay(
                                    "too many or duplicate companion requests".into(),
                                ));
                            }
                            let mut promise = requester.request(
                                schema::RelayAppToArkRequest { id, req },
                                clock.now() + EXCHANGE_TIMEOUT,
                            )?;
                            let answered = answered.clone();
                            let wake = shared.wake.clone();
                            promise.notify(move || {
                                let _ = answered.send(id);
                                let _ = wake.wake();
                            });
                            inbound.insert(id, promise);
                        }
                        Frame::Response(id, res) => {
                            if let Some((responder, deadline)) = pending.remove(&id) {
                                let _ = responder
                                    .reply(schema::RelayAppToArkResponse { id, res }, deadline)?;
                            }
                        }
                        Frame::Notice => {} // The wire has no notification or presence input.
                    },
                    Ok(Message::Pong(bytes)) => heartbeat.pong(&bytes, Instant::now()),
                    Ok(Message::Ping(_)) => {}
                    Ok(Message::Close(_)) => {
                        return Err(Failure::Relay("cloud closed the relay".into()));
                    }
                    Ok(_) => return Err(Failure::Relay("relay sent a non-binary message".into())),
                    Err(error) if would_block(&error) => break,
                    Err(error) => return Err(socket_error(error)),
                }
                batch_full = index == 31;
            }
            if let Some(ping) = heartbeat.ping(Instant::now())? {
                writing.start(Instant::now());
                match socket.write(Message::Ping(ping.to_vec().into())) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(socket_error(error)),
                }
            }
            match socket.flush() {
                Ok(()) => writing.flushed(),
                Err(error) if would_block(&error) => writing.start(Instant::now()),
                Err(error) => return Err(socket_error(error)),
            }
            if writing.expired(Instant::now()) {
                return Err(Failure::Wire(protocol::Error::Timeout));
            }
            let queued = !shared
                .state
                .lock()
                .expect("relay queue not poisoned")
                .queue
                .is_empty();
            if batch_full
                || (!writing.active()
                    && (!output.is_empty() || (queued && pending.len() < MAX_INFLIGHT)))
            {
                continue;
            }
            // Wait for readiness, a wakeup or the earliest deadline. Exchanges end
            // on the session's clock, the socket's own timers on real time.
            let (now, session) = (Instant::now(), clock.now());
            let timeout = pending
                .values()
                .map(|(_, deadline)| deadline.saturating_duration_since(session))
                .chain(writing.remaining(now))
                .chain(Some(heartbeat.deadline().saturating_duration_since(now)))
                .min();
            match poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(error)),
            }
        }
    })();
    let error = match result {
        Err(error) => crate::Error::from(error).to_string(),
        Ok(()) => "relay ended".into(),
    };
    shared.end(error.clone());
    for (_, (responder, _)) in pending {
        fail(responder, &error);
    }
}

/// Retains a bounded amount of output even when the peer stops reading.
fn enqueue(output: &mut VecDeque<Vec<u8>>, bytes: &mut usize, frame: Frame) -> Result<(), Failure> {
    let frame = frame.encode();
    if frame.len() > MAX_MESSAGE
        || output.len() >= MAX_INFLIGHT
        || frame.len() > MAX_BYTES.saturating_sub(*bytes)
    {
        return Err(Failure::Relay(
            "relay output queue full or message too large".into(),
        ));
    }
    *bytes += frame.len();
    output.push_back(frame);
    Ok(())
}

/// Complete prerequisite and forwarding paths over real wire peers.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::tests::{TIMEOUT, attach, response};
    use crate::testing::{Peer, answering, test_clock, wait_deadline};
    use crate::{Error, schema::host_to_ark::Content};
    use crate::{cloud::http, trust::Realm};
    use darkbio_clock::Clock;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tungstenite::handshake::server::{Request, Response};

    /// IDs retain all sixty-four bits, including numbers beyond JSON precision.
    const ID: u64 = (1 << 63) + 7;

    /// Accepts the next cloud connection, bounding its I/O in case a test fails.
    fn accept(listener: &TcpListener) -> TcpStream {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        stream
    }

    /// Reads headers without consuming any bytes of the first WebSocket frame.
    fn headers(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        String::from_utf8(request).unwrap()
    }

    /// Serves one sync followed by the requested number of relay attachments.
    fn cloud(
        joins: usize,
        mut serve: impl FnMut(usize, TcpStream) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            for (path, body) in [
                (
                    "/v1/cloudsync/identity",
                    r#"{"signer":"AQ==","crypto":"Ag=="}"#,
                ),
                (
                    "/v1/cloudsync/time?challenge=03",
                    r#"{"unixmilli":123,"signature":"BA=="}"#,
                ),
            ] {
                let mut stream = accept(&listener);
                assert!(headers(&mut stream).starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                stream.write_all(response(200, body).as_bytes()).unwrap();
            }
            for attempt in 0..joins {
                serve(attempt, accept(&listener));
            }
        });
        (url, worker)
    }

    /// Checks realm routing and the opaque authentication subprotocol.
    #[allow(clippy::result_large_err)] // Tungstenite requires a full HTTP rejection response.
    fn upgrade(stream: TcpStream) -> WebSocket<TcpStream> {
        tungstenite::accept_hdr(stream, |request: &Request, mut response: Response| {
            assert_eq!(request.uri().path(), "/v1/relaying");
            assert_eq!(
                request.headers()["Sec-WebSocket-Protocol"],
                "Relaying, Dark-Auth|-_8"
            );
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "Relaying".parse().unwrap());
            Ok(response)
        })
        .unwrap()
    }

    fn frame(socket: &mut WebSocket<TcpStream>) -> Frame {
        loop {
            match socket.read().unwrap() {
                Message::Binary(bytes) => return Frame::decode(&bytes).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected relay message: {other:?}"),
            }
        }
    }

    /// Makes the relay probe at attachment and again right after every pong,
    /// giving each pong `timeout`.
    fn probe_at_once(relay: &mut Relay, timeout: Duration) {
        let heartbeat = &mut relay.worker.as_mut().unwrap().heartbeat;
        let attached = heartbeat.deadline() - PING_INTERVAL;
        *heartbeat = Heartbeat::new(Duration::ZERO, timeout, attached);
    }

    /// Keeps serving app requests while an unlock waits for its reverse response.
    fn peer(clock: &Clock) -> (Peer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let joins = Arc::new(AtomicUsize::new(0));
        let syncs = Arc::new(AtomicUsize::new(0));
        let peer = Peer::spawn(
            clock,
            Box::new({
                let joins = joins.clone();
                let syncs = syncs.clone();
                let mut synced = false;
                let mut next_id = ID;
                move |session, request, responder| {
                    let deadline = session.clock().now() + TIMEOUT;
                    match request {
                        Content::DeviceInfo(_) => {
                            let clock = session
                                .clock()
                                .system_time()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            responder
                                .reply(
                                    schema::DeviceInfoResponse {
                                        cloud_clock: clock,
                                        cloud_synced: syncs.load(Ordering::SeqCst) > 0,
                                        ..Default::default()
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }

                        Content::CloudSyncStart(request) => {
                            assert_eq!(request.signer, [1]);
                            assert_eq!(request.crypto, [2]);
                            syncs.fetch_add(1, Ordering::SeqCst);
                            responder
                                .reply(
                                    schema::CloudSyncStartResponse { challenge: vec![3] },
                                    deadline,
                                )
                                .unwrap();
                        }
                        Content::CloudSyncFinish(request) => {
                            assert_eq!(request.unixmilli, 123);
                            assert_eq!(request.signature, [4]);
                            synced = true;
                            responder
                                .reply(schema::CloudSyncFinishResponse { accepted: 123 }, deadline)
                                .unwrap();
                        }
                        Content::RelayJoin(_) => {
                            assert!(synced);
                            joins.fetch_add(1, Ordering::SeqCst);
                            responder
                                .reply(
                                    schema::RelayJoinResponse {
                                        auth: vec![0xfb, 0xff],
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }
                        Content::ExecUploadStart(request) => {
                            assert!(synced);
                            assert_eq!(request.bytes, 4);
                            responder
                                .reply(
                                    schema::ExecutionUploadStartResponse { taskid: 42 },
                                    deadline,
                                )
                                .unwrap();
                        }
                        Content::ExecUploadChunk(request) => {
                            assert_eq!(request.taskid, 42);
                            assert_eq!(request.chunk, [0, 97, 115, 109]);
                            responder
                                .reply(schema::ExecutionUploadChunkResponse {}, deadline)
                                .unwrap();
                        }
                        Content::ExecStatus(request) => {
                            assert_eq!(request.taskid, 42);
                            responder
                                .reply(
                                    schema::ExecutionStatusResponse {
                                        pending: false,
                                        result: Some(schema::ExecutionResultResponse {
                                            success: true,
                                            ..Default::default()
                                        }),
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }
                        request @ (Content::Unlock(_)
                        | Content::ExecSched(_)
                        | Content::SlotRepair(_)
                        | Content::SlotDelete(_)
                        | Content::FirmwareUpdatePrep(_)
                        | Content::SlotUploadStart(_)) => {
                            let (reply, preflight, authorize): (protocol::Message, _, _) =
                                match request {
                                    Content::Unlock(_) => {
                                        (schema::UnlockResponse {}.into(), true, true)
                                    }
                                    Content::ExecSched(_) => {
                                        (schema::ExecutionScheduleResponse {}.into(), true, true)
                                    }
                                    Content::SlotRepair(_) => {
                                        (schema::SlotRepairResponse {}.into(), true, true)
                                    }
                                    Content::SlotDelete(_) => {
                                        (schema::SlotDeleteResponse {}.into(), true, true)
                                    }
                                    Content::FirmwareUpdatePrep(request) => (
                                        schema::FirmwareUpdatePrepResponse::default().into(),
                                        false,
                                        request.version != "unpaired",
                                    ),
                                    Content::SlotUploadStart(request) => (
                                        schema::SlotUploadStartResponse { session: 42 }.into(),
                                        false,
                                        request.kind() != schema::SlotKind::SlotReferenceGenome,
                                    ),
                                    _ => unreachable!(),
                                };
                            if !authorize {
                                responder.reply(reply, deadline).unwrap();
                                return true;
                            }
                            let id = next_id;
                            next_id += 1;
                            assert!(
                                !preflight || joins.load(Ordering::SeqCst) > 0,
                                "request preceded relay attachment"
                            );
                            let promise = session
                                .requester()
                                .request(
                                    schema::RelayArkToAppRequest {
                                        id,
                                        req: vec![1, 2, 3],
                                    },
                                    deadline,
                                )
                                .unwrap();
                            thread::spawn(move || {
                                let result = match promise.wait::<schema::RelayAppToArkResponse>() {
                                    Ok(answer) => {
                                        assert_eq!(answer.id, id);
                                        match answer.res.as_slice() {
                                            [4, 5, 6] => responder.reply(reply, deadline),
                                            [0] => responder.fail(
                                                schema::Error::new(0x506, "authorization denied"),
                                                deadline,
                                            ),
                                            _ => panic!("companion response was altered"),
                                        }
                                    }
                                    Err(protocol::Error::Remote(error)) => {
                                        responder.fail(error, deadline)
                                    }
                                    Err(_) => return,
                                };
                                let _ = result;
                            });
                        }
                        Content::RelayReq(request) => {
                            if request.req == [0] {
                                responder
                                    .fail(
                                        schema::Error::reserved(
                                            schema::ReservedErrors::Unsupported,
                                            "test refusal",
                                        ),
                                        deadline,
                                    )
                                    .unwrap();
                                return true;
                            }
                            assert_eq!(request.id, ID);
                            assert_eq!(request.req, [9, 8, 7]);
                            responder
                                .reply(
                                    schema::RelayArkToAppResponse {
                                        id: ID,
                                        res: vec![6, 5, 4],
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }
                        other => return answering(session, other, responder),
                    }
                    true
                }
            }),
        );
        (peer, joins, syncs)
    }

    /// Setup, simultaneous traffic in both directions, approval, denial and reuse
    /// all run without an application receive loop.
    #[test]
    fn test_unlock() {
        let (release, pause) = mpsc::channel();
        let (staged, stages) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            let mut socket = upgrade(stream);
            socket
                .send(Message::Binary(
                    cbor::encode(Envelope {
                        darkrpc: 1,
                        presence: Some(vec![0]),
                        ..Default::default()
                    })
                    .unwrap()
                    .into(),
                ))
                .unwrap();
            socket.send(Message::Ping(vec![1].into())).unwrap();
            socket
                .send(Message::Binary(
                    Frame::Request(ID, vec![9, 8, 7]).encode().into(),
                ))
                .unwrap();
            let mut requests = 0;
            let mut responses = 0;
            for _ in 0..2 {
                match frame(&mut socket) {
                    Frame::Request(id, req) => {
                        requests += 1;
                        assert_eq!((id, req), (ID, vec![1, 2, 3]));
                        socket
                            .send(Message::Binary(
                                Frame::Response(id, vec![4, 5, 6]).encode().into(),
                            ))
                            .unwrap();
                    }
                    Frame::Response(id, res) => {
                        responses += 1;
                        assert_eq!((id, res), (ID, vec![6, 5, 4]));
                    }
                    Frame::Notice => panic!("host originated a notification"),
                }
            }
            assert_eq!((requests, responses), (1, 1));
            staged.send(()).unwrap();
            let Frame::Request(id, _) = frame(&mut socket) else {
                panic!("expected second unlock")
            };
            socket
                .send(Message::Binary(
                    Frame::Response(id, vec![0]).encode().into(),
                ))
                .unwrap();
            pause.recv().unwrap();
        });
        let clock = test_clock().clock();
        let (mut peer, joins, syncs) = peer(&clock);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
        assert_eq!(joins.load(Ordering::SeqCst), 0);
        assert_eq!(syncs.load(Ordering::SeqCst), 0);
        client.call(schema::UnlockRequest {}, deadline).unwrap();
        stages.recv().unwrap();
        assert!(
            matches!(client.clone().call(schema::UnlockRequest {}, deadline), Err(Error::Remote(error)) if error.code == 0x506)
        );
        assert_eq!(joins.load(Ordering::SeqCst), 1);
        assert_eq!(syncs.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// A refused companion request leaves a simultaneous authorization and a
    /// second companion exchange on the same attachment intact.
    #[test]
    fn test_request_refusal() {
        let (release, pause) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            let mut socket = upgrade(stream);
            let Frame::Request(id, _) = frame(&mut socket) else {
                panic!("expected authorization")
            };
            for request in [
                Frame::Request(ID + 1, vec![0]),
                Frame::Request(ID, vec![9, 8, 7]),
            ] {
                socket
                    .send(Message::Binary(request.encode().into()))
                    .unwrap();
            }
            assert!(matches!(frame(&mut socket), Frame::Response(ID, bytes) if bytes == [6, 5, 4]));
            socket
                .send(Message::Binary(
                    Frame::Response(id, vec![4, 5, 6]).encode().into(),
                ))
                .unwrap();
            pause.recv().unwrap();
        });
        let (mut peer, joins, _) = peer(&test_clock().clock());
        let ark = attach(&mut peer, url);
        ark.client()
            .call_timeout(schema::UnlockRequest {}, TIMEOUT)
            .unwrap();
        assert_eq!(joins.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// Every unconditional authorization establishes the relay before reaching
    /// the Ark, without relying on an earlier unlock on this connection.
    #[test]
    fn test_authorization_prerequisites() {
        type Call = fn(&crate::Client) -> Result<(), Error>;
        let calls: [Call; 4] = [
            |client| {
                client
                    .call_timeout(schema::ExecutionScheduleRequest::default(), TIMEOUT)
                    .map(drop)
            },
            |client| {
                client
                    .call_timeout(schema::SlotRepairRequest::default(), TIMEOUT)
                    .map(drop)
            },
            |client| {
                client
                    .call_timeout(schema::SlotDeleteRequest::default(), TIMEOUT)
                    .map(drop)
            },
            |client| {
                let result = client.execute(
                    4,
                    &mut [0, 97, 115, 109].as_slice(),
                    client.clock().now() + TIMEOUT,
                    |_| {},
                )?;
                assert!(result.success);
                Ok(())
            },
        ];
        let clock = test_clock().clock();
        for call in calls {
            let (release, pause) = mpsc::channel();
            let (url, server) = cloud(1, move |_, stream| {
                let mut socket = upgrade(stream);
                let Frame::Request(id, _) = frame(&mut socket) else {
                    panic!("expected authorization")
                };
                socket
                    .send(Message::Binary(
                        Frame::Response(id, vec![4, 5, 6]).encode().into(),
                    ))
                    .unwrap();
                pause.recv().unwrap();
            });
            let (mut peer, joins, _) = peer(&clock);
            let ark = attach(&mut peer, url);
            call(&ark.client()).unwrap();
            assert_eq!(joins.load(Ordering::SeqCst), 1);
            release.send(()).unwrap();
            server.join().unwrap();
        }
    }

    /// Unpaired updates and catalog uploads need no companion. Their conditional
    /// counterparts attach when the Ark first requests authorization.
    #[test]
    fn test_conditional_authorization() {
        let clock = test_clock().clock();
        for firmware in [false, true] {
            let (release, pause) = mpsc::channel();
            let (url, server) = cloud(1, move |_, stream| {
                let mut socket = upgrade(stream);
                let Frame::Request(id, _) = frame(&mut socket) else {
                    panic!("expected authorization")
                };
                socket
                    .send(Message::Binary(
                        Frame::Response(id, vec![4, 5, 6]).encode().into(),
                    ))
                    .unwrap();
                pause.recv().unwrap();
            });
            let (mut peer, joins, _) = peer(&clock);
            let ark = attach(&mut peer, url);
            let client = ark.client();
            for authorize in [false, true] {
                if firmware {
                    client
                        .call_timeout(
                            schema::FirmwareUpdatePrepRequest {
                                version: if authorize { "paired" } else { "unpaired" }.into(),
                                ..Default::default()
                            },
                            TIMEOUT,
                        )
                        .unwrap();
                } else {
                    client
                        .call_timeout(
                            schema::SlotUploadStartRequest {
                                kind: if authorize {
                                    schema::SlotKind::SlotSnpIndelCalls
                                } else {
                                    schema::SlotKind::SlotReferenceGenome
                                } as i32,
                                ..Default::default()
                            },
                            TIMEOUT,
                        )
                        .unwrap();
                }
                assert_eq!(joins.load(Ordering::SeqCst), usize::from(authorize));
            }
            release.send(()).unwrap();
            server.join().unwrap();
        }
    }

    /// Client clones share one attachment. A shorter caller expires independently
    /// while local requests and two concurrent authorizations remain available.
    #[test]
    fn test_shared_attachment() {
        // Hold the leader's relay attachment at its upgrade
        let (seen, attempts) = mpsc::channel();
        let (release, pause) = mpsc::channel();
        let (finish, done) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            seen.send(()).unwrap();
            pause.recv().unwrap();
            let mut socket = upgrade(stream);
            for _ in 0..2 {
                let Frame::Request(id, req) = frame(&mut socket) else {
                    panic!("expected unlock")
                };
                assert_eq!(req, [1, 2, 3]);
                socket
                    .send(Message::Binary(
                        Frame::Response(id, vec![4, 5, 6]).encode().into(),
                    ))
                    .unwrap();
            }
            done.recv().unwrap();
        });
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut peer, joins, syncs) = peer(&clock);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(schema::UnlockRequest {}, deadline)
        });
        attempts.recv().unwrap();
        client.call(schema::DeviceInfoRequest {}, deadline).unwrap();

        // A short caller joining the attachment expires alone, at the earliest
        // deadline on the clock
        let short = clock.now() + Duration::from_millis(20);
        let waiter = thread::spawn({
            let client = client.clone();
            move || client.call_timeout(schema::UnlockRequest {}, Duration::from_millis(20))
        });
        wait_deadline(&tester, short);
        tester.advance_to(short);
        assert!(matches!(waiter.join().unwrap(), Err(Error::Timeout)));

        // The leader and a later caller authorize over the one attachment
        let follower = thread::spawn({
            let client = client.clone();
            move || client.call(schema::UnlockRequest {}, deadline)
        });
        release.send(()).unwrap();
        leader.join().unwrap().unwrap();
        follower.join().unwrap().unwrap();
        assert_eq!(joins.load(Ordering::SeqCst), 1);
        assert_eq!(syncs.load(Ordering::SeqCst), 1);
        finish.send(()).unwrap();
        server.join().unwrap();
    }

    /// A call deadline still bounds authorization after successful cloud setup.
    #[test]
    fn test_authorization_deadline() {
        // The companion receives the authorization request but never answers
        let (reached, authorizing) = mpsc::channel();
        let (release, pause) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            let mut socket = upgrade(stream);
            assert!(matches!(frame(&mut socket), Frame::Request(_, _)));
            reached.send(()).unwrap();
            pause.recv().unwrap();
        });
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut peer, _, _) = peer(&clock);
        let ark = attach(&mut peer, url);
        let client = ark.client();

        // The unlock waits for approval until the clock reaches its deadline
        let deadline = clock.now() + Duration::from_millis(500);
        let unlock = thread::spawn({
            let client = client.clone();
            move || client.call(schema::UnlockRequest {}, deadline)
        });
        authorizing.recv().unwrap();
        tester.advance_to(deadline);
        assert!(matches!(unlock.join().unwrap(), Err(Error::Timeout)));

        // Local requests keep working on the same connection
        client
            .call_timeout(schema::DeviceInfoRequest {}, TIMEOUT)
            .unwrap();
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// Refused upgrades and broken relays permit another attachment, without
    /// replaying the unlock or disrupting local device requests.
    #[test]
    fn test_reconnect() {
        let clock = test_clock().clock();
        for refused in [false, true] {
            let (release, pause) = mpsc::channel();
            let (url, server) = cloud(2, move |attempt, mut stream| {
                if attempt == 0 && refused {
                    headers(&mut stream);
                    stream
                        .write_all(response(503, "unavailable").as_bytes())
                        .unwrap();
                    return;
                }
                let mut socket = upgrade(stream);
                let Frame::Request(id, _) = frame(&mut socket) else {
                    panic!("expected unlock")
                };
                if attempt == 0 {
                    socket.close(None).unwrap();
                } else {
                    socket
                        .send(Message::Binary(
                            Frame::Response(id, vec![4, 5, 6]).encode().into(),
                        ))
                        .unwrap();
                    pause.recv().unwrap();
                }
            });
            let (mut peer, joins, syncs) = peer(&clock);
            let ark = attach(&mut peer, url);
            let client = ark.client();
            let deadline = clock.now() + TIMEOUT;
            let error = client.call(schema::UnlockRequest {}, deadline).unwrap_err();
            if refused {
                assert!(matches!(error, Error::Cloud(message) if message.contains("503")));
            } else {
                assert!(
                    matches!(error, Error::Remote(error) if error.code == schema::ReservedErrors::Unavailable as u64)
                );
            }
            client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
            client
                .call(schema::SlotDeleteRequest::default(), deadline)
                .unwrap();
            assert_eq!(joins.load(Ordering::SeqCst), 2);
            assert_eq!(syncs.load(Ordering::SeqCst), 1);
            release.send(()).unwrap();
            server.join().unwrap();
        }
    }

    /// Dropping the owner ends authorization and the socket despite a retained client.
    #[test]
    fn test_owner_close() {
        let (seen, requests) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            let mut socket = upgrade(stream);
            assert!(matches!(frame(&mut socket), Frame::Request(_, _)));
            seen.send(()).unwrap();
            assert!(matches!(socket.read(), Err(_) | Ok(Message::Close(_))));
        });
        let clock = test_clock().clock();
        let (mut peer, _, _) = peer(&clock);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let pending = client
            .send(schema::UnlockRequest {}, clock.now() + TIMEOUT)
            .unwrap();
        requests.recv().unwrap();
        drop(ark);
        assert!(matches!(pending.wait(), Err(Error::Closed)));
        assert!(matches!(
            client.call(schema::UnlockRequest {}, clock.now() + TIMEOUT),
            Err(Error::Closed)
        ));
        server.join().unwrap();
    }

    /// Unsolicited or late pongs cannot acknowledge a different probe or restart
    /// its deadline. A valid pong schedules a fresh probe with a different ID.
    #[test]
    fn test_heartbeat() {
        // Probe first once an interval has passed since the start
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut heartbeat = Heartbeat::new(PING_INTERVAL, PONG_TIMEOUT, clock.now());
        assert!(heartbeat.ping(clock.now()).unwrap().is_none());
        tester.advance(PING_INTERVAL);
        let now = clock.now();
        let first = heartbeat.ping(now).unwrap().unwrap();
        let deadline = heartbeat.deadline();
        assert!(heartbeat.ping(now).unwrap().is_none());

        // Only the pong echoing the probe acknowledges it and schedules the next
        heartbeat.pong(&[0; 8], now);
        assert_eq!(heartbeat.deadline(), deadline);
        heartbeat.pong(&first, now);
        assert_eq!(heartbeat.deadline(), now + PING_INTERVAL);

        // An old probe's pong or one arriving at the deadline leaves the probe to expire
        tester.advance(PING_INTERVAL);
        let second = heartbeat.ping(clock.now()).unwrap().unwrap();
        assert_ne!(first, second);
        let deadline = heartbeat.deadline();
        tester.advance_to(deadline - Duration::from_millis(1));
        heartbeat.pong(&first, clock.now());
        tester.advance_to(deadline);
        heartbeat.pong(&second, clock.now());
        assert!(heartbeat.ping(clock.now()).is_err());
    }

    /// Output left to flush starts the backlog bound once. Later blockage keeps
    /// the original deadline, where the bound expires, and a completed flush
    /// clears it for the next output.
    #[test]
    fn test_backlog() {
        // The first blockage starts the bound
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut backlog = Backlog::default();
        assert!(!backlog.active());
        let start = clock.now();
        backlog.start(start);
        assert!(backlog.active());
        assert_eq!(backlog.remaining(start), Some(WRITE_TIMEOUT));

        // Blocking again keeps the original deadline, which ends the bound on time
        tester.advance(Duration::from_secs(3));
        backlog.start(clock.now());
        assert_eq!(
            backlog.remaining(clock.now()),
            Some(WRITE_TIMEOUT - Duration::from_secs(3))
        );
        tester.advance_to(start + WRITE_TIMEOUT - Duration::from_millis(1));
        assert!(!backlog.expired(clock.now()));
        tester.advance_to(start + WRITE_TIMEOUT);
        assert!(backlog.expired(clock.now()));

        // A completed flush clears the bound, and the next output starts afresh
        backlog.flushed();
        assert!(!backlog.active());
        assert!(!backlog.expired(clock.now()));
        backlog.start(clock.now());
        assert_eq!(backlog.remaining(clock.now()), Some(WRITE_TIMEOUT));
    }

    /// The first probe is due an interval after attachment, so a worker that
    /// starts that late probes at once.
    #[test]
    fn test_heartbeat_starts_at_attachment() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (probed, probes) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert!(matches!(socket.read().unwrap(), Message::Ping(_)));
            probed.send(()).unwrap();
            let _ = socket.get_mut().read_to_end(&mut Vec::new());
        });
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let api = http::tests::api(url.clone(), Realm::Hardware, &clock);
        let deadline = clock.now() + TIMEOUT;
        let mut relay =
            Relay::connect(&api, &url, &[0xfb, 0xff], session.requester(), deadline).unwrap();

        // Start the worker as if a whole interval passed since attachment
        relay.worker.as_mut().unwrap().heartbeat.next -= PING_INTERVAL;
        relay.start().unwrap();
        probes.recv().unwrap();
        relay.close();
        server.join().unwrap();
    }

    /// The socket pump accepts matching pongs, then ends an unresponsive relay
    /// without disrupting local wire calls. A replacement can attach afterward.
    /// Both relays probe at once. The first gives every pong an hour, and the
    /// test waits for the server to answer three probes. The second gives its
    /// pong no time, and the test waits for the server to see its socket end.
    #[test]
    fn test_heartbeat_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (answered, pongs) = mpsc::channel();
        let (gone, ended) = mpsc::channel();
        let (release, pause) = mpsc::channel();
        let server = thread::spawn(move || {
            // Answer three probes, each one sent only once the previous pong matched
            let mut socket = upgrade(accept(&listener));
            for _ in 0..3 {
                assert!(matches!(socket.read().unwrap(), Message::Ping(_)));
                socket.flush().unwrap();
            }
            answered.send(()).unwrap();
            let _ = socket.get_mut().read_to_end(&mut Vec::new());

            // Leave the next relay's probe unanswered until the relay goes away
            let mut socket = upgrade(accept(&listener));
            let _ = socket.get_mut().read_to_end(&mut Vec::new());
            gone.send(()).unwrap();

            // Hold a replacement attachment open until the test ends
            let _replacement = upgrade(accept(&listener));
            pause.recv().unwrap();
        });
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let api = http::tests::api(url.clone(), Realm::Hardware, &clock);
        let deadline = clock.now() + TIMEOUT;

        // A relay whose probes are answered keeps probing
        let mut relay =
            Relay::connect(&api, &url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
        probe_at_once(&mut relay, Duration::from_secs(3600));
        relay.start().unwrap();
        pongs.recv().unwrap();
        relay.close();

        // A relay whose probe expires at once ends, leaving the wire session usable
        let mut relay =
            Relay::connect(&api, &url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
        probe_at_once(&mut relay, Duration::ZERO);
        relay.start().unwrap();
        ended.recv().unwrap();
        let error = relay.shared.state.lock().unwrap().error.clone();
        assert_eq!(
            error.as_deref(),
            Some("relay operation failed: cloud relay heartbeat timed out")
        );
        session
            .requester()
            .request(schema::DeviceInfoRequest {}, deadline)
            .unwrap()
            .wait::<schema::DeviceInfoResponse>()
            .unwrap();

        // A replacement attaches afterward
        let mut replacement =
            Relay::connect(&api, &url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
        replacement.start().unwrap();
        assert!(replacement.connected());
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// Closing the attachment shuts down TCP even before its worker starts or
    /// while it is assembling an unfinished fragmented message.
    #[test]
    fn test_close_fragmented_message() {
        let clock = test_clock().clock();
        for started in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
            let (sent, fragments) = mpsc::channel();
            let server = thread::spawn(move || {
                let mut socket = upgrade(accept(&listener));
                let mut bytes = vec![0; 128 * 1024];
                bytes[0] = 2; // Non-final binary frame followed by empty continuations
                socket.get_mut().write_all(&bytes).unwrap();
                sent.send(()).unwrap();
                assert!(matches!(socket.get_mut().read(&mut [0]), Ok(0) | Err(_)));
            });
            let mut peer = Peer::spawn(&clock, Box::new(answering));
            let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let deadline = clock.now() + TIMEOUT;
            let mut relay = Relay::connect(
                &http::tests::api(url.clone(), Realm::Hardware, &clock),
                &url,
                &[0xfb, 0xff],
                session.requester(),
                deadline,
            )
            .unwrap();
            let (dropped, released) = mpsc::channel();
            *relay.shared.dropped.lock().unwrap() = Some(dropped);
            if started {
                relay.start().unwrap();
            }
            fragments.recv().unwrap();
            relay.close();
            server.join().unwrap();

            // The worker exits, so dropping this attachment releases the last handle
            drop(relay);
            released.recv().unwrap();
        }
    }

    /// Every read of a stalled upgrade retains the caller's original deadline.
    /// The socket's own timeout is real, so the upgrade stalls for the whole
    /// 100 ms budget that the clock leaves it.
    #[test]
    fn test_handshake_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (release, pause) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            headers(&mut stream);
            pause.recv().unwrap();
        });
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let result = Relay::connect(
            &http::tests::api(url.clone(), Realm::Hardware, &clock),
            &url,
            &[1],
            session.requester(),
            clock.now() + Duration::from_millis(100),
        );
        assert!(
            matches!(result, Err(Failure::Wire(protocol::Error::Timeout))),
            "{result:?}"
        );
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// Ambiguous, malformed and foreign-version envelopes never reach the Ark.
    #[test]
    fn test_envelopes() {
        let encoded = Frame::Request(ID, vec![0xfb, 0xff]).encode();
        assert_eq!(hex::encode(&encoded), "86011b8000000000000007f642fbfff6f6");
        assert!(
            matches!(Frame::decode(&encoded).unwrap(), Frame::Request(ID, bytes) if bytes == [0xfb, 0xff])
        );
        for envelope in [
            Envelope {
                darkrpc: 2,
                id: Some(ID),
                request: Some(vec![]),
                ..Default::default()
            },
            Envelope {
                darkrpc: 1,
                request: Some(vec![]),
                ..Default::default()
            },
            Envelope {
                darkrpc: 1,
                id: Some(ID),
                request: Some(vec![]),
                response: Some(vec![]),
                ..Default::default()
            },
            Envelope {
                darkrpc: 1,
                id: Some(ID),
                presence: Some(vec![]),
                ..Default::default()
            },
        ] {
            assert!(Frame::decode(&cbor::encode(envelope).unwrap()).is_err());
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert!(Frame::decode(&trailing).is_err());
        assert!(Frame::decode(&[0xff]).is_err());
    }
}
