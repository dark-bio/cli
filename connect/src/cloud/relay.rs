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

/// Largest encoded relay envelope, the most one wire message carries.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;
/// Byte allowance, 16 MiB, for each of the admission and output queues, not
/// counting messages in flight on the wire.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Request count limit, 128, for the admission queue, the open exchanges in
/// each direction and the output queue.
const MAX_INFLIGHT: usize = 128;

/// Time an Ark request or a companion request stays open on the relay, 60 s.
///
/// An Ark request that needs the relay attached spends part of it attaching.
/// It also releases a responder whose companion never answers, whatever the
/// caller's own deadline.
pub(super) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time, 5 s, that written output may wait to flush through the cloud
/// socket.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval, 15 s, from attachment or a matching pong to the next liveness
/// probe.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Maximum wait, 10 s, for the pong matching the current probe.
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// Readiness token for incoming traffic and pending socket writes.
const SOCKET: Token = Token(0);
/// Readiness token for queued Ark requests, wire completions and local closure.
const WAKE: Token = Token(1);

/// Relay attached to one connection, ending its worker and forwarding when
/// dropped.
#[derive(Debug)]
pub(super) struct Relay {
    /// Admission queue and ending reason, shared by the dispatcher and the
    /// worker.
    shared: Arc<Shared>,
    /// Worker resources, until [`Self::start`] moves them into the worker
    /// thread.
    worker: Option<Worker>,
}

/// Connected resources that [`Relay::start`] moves into the worker thread.
#[derive(Debug)]
struct Worker {
    /// Sole owner of WebSocket framing and TLS state.
    socket: WebSocket<MaybeTlsStream<Socket>>,
    /// Poller for socket readiness, admission wakeups and exchange deadlines.
    poll: Poll,
    /// Requester passing companion requests to the Ark over the original
    /// session.
    requester: Requester,
    /// Cloud liveness probes, independent of companion availability.
    heartbeat: Heartbeat,
}

/// Admission and shutdown handles shared by the dispatcher and relay worker.
#[derive(Debug)]
struct Shared {
    /// Queue and ending reason under one lock, so admission and closure never
    /// interleave.
    state: Mutex<State>,
    /// Waker for queued Ark requests, wire completions and local closure.
    wake: Arc<Waker>,
    /// Clone of the TCP stream, shut down to interrupt the worker even inside
    /// WebSocket message assembly.
    socket: TcpStream,
    /// Channel notifying a test once the last handle to this relay is gone.
    #[cfg(test)]
    dropped: Mutex<Option<mpsc::Sender<()>>>,
}

/// Ark requests waiting for the worker, and the first reason the relay ended.
#[derive(Debug, Default)]
struct State {
    /// Ark requests, unanswered responders and their fixed exchange deadlines.
    queue: VecDeque<(schema::RelayArkToAppRequest, Responder, Instant)>,
    /// Opaque request bytes waiting in the queue.
    bytes: usize,
    /// First reason this relay ended.
    error: Option<String>,
}

impl Relay {
    /// Opens an authenticated relay socket under the operation's deadline,
    /// ready for [`Self::start`] to hand to the worker.
    pub(super) fn connect(
        api: &super::http::Api,
        url: &str,
        auth: &[u8],
        requester: Requester,
        deadline: Instant,
    ) -> Result<Self, Failure> {
        let mut socket = socket::connect(api, url, auth, "Relaying", deadline)?;
        api.clock.remaining(deadline).map_err(io_error)?;

        // Switch the upgraded stream to readiness polling, keeping a clone to
        // shut it down with
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

        // Share the queue and shutdown handles, holding the worker's resources
        // until it starts
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

    /// Starts the worker thread that services this relay.
    ///
    /// The setup calls it under its lock, so dispatch finds this relay for any
    /// Ark request that an early companion message provokes.
    ///
    /// # Panics
    ///
    /// Panics if the worker already started.
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

    /// Checks whether this attachment has not recorded an ending yet.
    ///
    /// This is a local snapshot, so a dead peer may go unnoticed until I/O
    /// fails or the next heartbeat expires.
    pub(super) fn connected(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("relay queue not poisoned")
            .error
            .is_none()
    }

    /// Queues an Ark request for the companion without blocking dispatch.
    ///
    /// A relay that ended, a full queue or a body without 64 bytes of room for
    /// its envelope refuses the request at once with `UNAVAILABLE`.
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

    /// Ends the relay, refusing queued requests and waking the worker without
    /// waiting for it to exit.
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
    /// Ends the relay with `error`, refusing queued requests and interrupting
    /// the socket.
    ///
    /// Only the first reason is kept, and later calls do nothing.
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

/// Checks whether a WebSocket error is a nonblocking operation left for the
/// poll loop to resume.
fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

/// Refuses an Ark request with `UNAVAILABLE` and the reason, queueing the reply
/// without waiting on I/O.
///
/// The reply gets [`protocol::DEFAULT_AUTOREPLY_TIMEOUT`] to go out.
pub(super) fn fail(responder: Responder, reason: &str) {
    let error = schema::Error::reserved(schema::ReservedErrors::Unavailable, reason);
    let deadline = responder.clock().now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT;
    let _ = responder.fail(error, deadline);
}

/// Cloud relay envelope, whose framing is all the host reads.
///
/// Every body stays opaque to the host.
#[derive(Cbor, Default)]
#[cbor(array)]
struct Envelope {
    /// Envelope version, which must be `1`.
    darkrpc: u64,
    /// Correlation ID present only on requests and responses.
    id: Option<u64>,
    /// Sealed notification body, accepted and ignored since the wire has no
    /// input for it.
    notify: Option<Vec<u8>>,
    /// Sealed request body forwarded without interpretation.
    request: Option<Vec<u8>>,
    /// Sealed response body matched to an outstanding request.
    response: Option<Vec<u8>>,
    /// Presence body, accepted and ignored since the wire has no input for it.
    presence: Option<Vec<u8>>,
}

/// Envelope validated into one of its three shapes.
///
/// The host originates only requests and responses.
enum Frame {
    /// Correlation ID and opaque request bytes.
    Request(u64, Vec<u8>),
    /// Correlation ID and opaque response bytes.
    Response(u64, Vec<u8>),
    /// Recognized notification or presence envelope with no wire destination.
    Notice,
}

impl Frame {
    /// Decodes an envelope, requiring version `1` and exactly one body, with an
    /// ID only on requests and responses.
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

    /// Encodes a request or response envelope, keeping its ID and sealed body
    /// unchanged.
    ///
    /// # Panics
    ///
    /// Panics on [`Self::Notice`], since the host originates no notifications.
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

/// Liveness probe schedule for the cloud socket, independent of companion
/// traffic.
///
/// Only a pong echoing the current ping proves liveness, and other traffic
/// cannot defer a probe.
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
    /// Schedules the first probe of a relay attaching now.
    ///
    /// The worker's socket timers run on real time, so the schedule starts on
    /// it too.
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

    /// Returns a probe payload when one is due, keeping at most one probe
    /// outstanding.
    ///
    /// Fails once the outstanding probe's pong is overdue.
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

    /// Returns the next instant the worker must wake, to send a probe or detect
    /// its expiry.
    fn deadline(&self) -> Instant {
        self.pending.map_or(self.next, |(_, deadline)| deadline)
    }
}

/// Time bound on output the cloud socket has not taken yet.
///
/// The first write or deferred flush starts the bound, later ones keep its
/// deadline, and only a completed flush ends it.
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

    /// Checks whether output is left to flush, which holds back the next frame.
    fn active(&self) -> bool {
        self.deadline.is_some()
    }

    /// Checks whether the output left to flush missed its deadline by `now`.
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Returns the time left after `now` before the deadline, which bounds the
    /// next poll.
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }
}

/// Runs the relay worker, writing one frame at a time while receiving
/// concurrently.
///
/// Wire completions and queued Ark requests wake the poll, and idle wakeups
/// check liveness. The socket's own timers read real time, while exchange
/// deadlines read the session's clock. When the relay ends, every Ark request
/// still queued or open is refused with the ending reason.
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

    // Output waits in a bounded queue and goes out one frame at a time
    let mut output = VecDeque::new();
    let mut bytes = 0usize;
    let mut writing = Backlog::default();

    // Service the relay until the first failure ends it
    let result = (|| -> Result<(), Failure> {
        loop {
            // Stop once the relay ended, or admit one queued Ark request
            {
                let mut state = shared.state.lock().expect("relay queue not poisoned");
                if let Some(error) = &state.error {
                    return Err(Failure::Relay(error.clone()));
                }
                // Admission stops while the socket is backlogged, but reading
                // and completion handling continue independently of that backlog
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

            // Refuse Ark requests whose exchange expired, even if no further
            // traffic arrives
            let now = clock.now();
            let expired: Vec<_> = pending
                .iter()
                .filter(|(_, (_, deadline))| now >= *deadline)
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                fail(pending.remove(&id).unwrap().0, "relay request timed out");
            }

            // Pass the Ark's answers to companion requests on to the cloud
            while let Ok(id) = answers.try_recv() {
                if let Some(promise) = inbound.remove(&id) {
                    let response = match promise.wait::<schema::RelayArkToAppResponse>() {
                        Ok(response) => response,
                        // Only the Ark can seal an answer for the companion, so
                        // this one goes unanswered and unrelated ones continue
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

            // Start writing the next frame once the previous one is flushed
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
            // expired Ark requests or the heartbeat
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

                            // Ask the Ark, waking the worker on its answer
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
                            // Responses to closed exchanges are dropped
                            if let Some((responder, deadline)) = pending.remove(&id) {
                                let _ = responder
                                    .reply(schema::RelayAppToArkResponse { id, res }, deadline)?;
                            }
                        }
                        Frame::Notice => {} // the wire has no notification or presence input
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

            // Probe the cloud when due, failing once a probe went unanswered
            if let Some(ping) = heartbeat.ping(Instant::now())? {
                writing.start(Instant::now());
                match socket.write(Message::Ping(ping.to_vec().into())) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(socket_error(error)),
                }
            }

            // Flush, ending the relay once output stays backlogged too long
            match socket.flush() {
                Ok(()) => writing.flushed(),
                Err(error) if would_block(&error) => writing.start(Instant::now()),
                Err(error) => return Err(socket_error(error)),
            }
            if writing.expired(Instant::now()) {
                return Err(Failure::Wire(protocol::Error::Timeout));
            }

            // Loop again at once while work is ready, without polling
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

    // Refuse every Ark request still queued or open with the ending reason
    let error = match result {
        Err(error) => crate::Error::from(error).to_string(),
        Ok(()) => "relay ended".into(),
    };
    shared.end(error.clone());
    for (_, (responder, _)) in pending {
        fail(responder, &error);
    }
}

/// Queues an encoded frame for the socket, keeping the output bounded when the
/// cloud stops reading.
///
/// A frame that does not fit fails, which ends the relay.
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

    /// Relay ID with its top bit set, beyond what a JSON number carries exactly.
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

    /// Accepts a relay upgrade after checking its route and the proof in its
    /// subprotocol header.
    #[allow(clippy::result_large_err)] // the upgrade callback's error is a whole HTTP response
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

    /// Reads the next binary message as a relay envelope, skipping pings and
    /// pongs.
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

    /// Spawns an Ark peer that asks the companion to authorize guarded requests,
    /// serving other requests while an authorization waits.
    ///
    /// It counts relay joins and cloud syncs, approves on a `[4, 5, 6]` answer
    /// and denies on `[0]`.
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
                            // Pick the reply, whether the relay must already be
                            // attached, and whether the companion must authorize
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

                            // Ask the companion, answering from another thread
                            // so this peer keeps serving meanwhile
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
                            // Refuse a `[0]` request and answer the expected one
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
            // Send a presence notice, a ping and a companion request right after
            // the upgrade
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

            // Approve the unlock while the Ark's answer to the companion arrives
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

            // Deny the next unlock once the test is ready for it
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

        // Status needs neither sync nor relay
        let clock = test_clock().clock();
        let (mut peer, joins, syncs) = peer(&clock);
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = clock.now() + TIMEOUT;
        client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
        assert_eq!(joins.load(Ordering::SeqCst), 0);
        assert_eq!(syncs.load(Ordering::SeqCst), 0);

        // The first unlock attaches the relay and is approved, and the second
        // reuses the attachment and is denied
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

            // While the authorization waits, send a companion request the Ark
            // refuses and one it serves, then approve the authorization
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
        /// Guarded operation under test, run on a fresh connection.
        type Call = fn(&crate::Client) -> Result<(), Error>;

        // Scheduling, repair, deletion and a whole app run each need approval
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

        // Each runs on a fresh connection whose companion approves once
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

    /// Conditional requests attach the relay only once the Ark asks for
    /// authorization, and not when it answers them directly.
    #[test]
    fn test_conditional_authorization() {
        let clock = test_clock().clock();
        for firmware in [false, true] {
            // Serve one attachment whose companion approves once
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

            // A request the Ark answers directly attaches nothing, and one it
            // authorizes attaches the relay
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

    /// Client clones share one attachment, and a shorter caller expires alone
    /// while local requests and concurrent authorizations continue.
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

        // Status runs while the attachment is held
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
            // Refuse or break the first attachment, then approve over the second
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

            // The unlock fails with the first attachment and is not replayed
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

            // Local requests go on, and the next guarded request attaches again
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

    /// Dropping the owner ends authorization and the socket despite a retained
    /// client.
    #[test]
    fn test_owner_close() {
        // Hold an unlock's authorization at the companion until the socket ends
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

        // Drop the owner while the authorization waits
        drop(ark);
        assert!(matches!(pending.wait(), Err(Error::Closed)));
        assert!(matches!(
            client.call(schema::UnlockRequest {}, clock.now() + TIMEOUT),
            Err(Error::Closed)
        ));
        server.join().unwrap();
    }

    /// Only a timely pong echoing the current probe acknowledges it, and the
    /// next probe carries a new ID.
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

        // An old probe's pong, or one arriving at the deadline, leaves the probe
        // to expire
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

    /// The backlog bound starts once, expires on its original deadline and
    /// clears with a completed flush.
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
        // Attach a relay to a cloud that waits for the first probe
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

    /// The worker keeps a relay whose pongs match, ends one whose probe expires,
    /// and leaves the wire session usable for a replacement.
    #[test]
    fn test_heartbeat_disconnect() {
        // Serve three attachments, one after another
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (answered, pongs) = mpsc::channel();
        let (gone, ended) = mpsc::channel();
        let (release, pause) = mpsc::channel();
        let server = thread::spawn(move || {
            // Answer three probes, each sent only once the previous pong matched
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

        // Open the wire session that every relay attaches beside
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

        // A relay whose probe expires at once ends, and the wire session stays
        // usable
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
            // Serve a relay that starts a fragmented message and never ends it
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
            let (sent, fragments) = mpsc::channel();
            let server = thread::spawn(move || {
                let mut socket = upgrade(accept(&listener));
                let mut bytes = vec![0; 128 * 1024];
                bytes[0] = 2; // non-final binary frame followed by empty continuations
                socket.get_mut().write_all(&bytes).unwrap();
                sent.send(()).unwrap();
                assert!(matches!(socket.get_mut().read(&mut [0]), Ok(0) | Err(_)));
            });

            // Close amid the unfinished message, before or after the worker starts
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

    /// Every read of a stalled upgrade keeps the caller's original deadline.
    ///
    /// The socket's own timeout runs on real time, so the upgrade stalls for
    /// the whole 100 ms that the clock leaves it.
    #[test]
    fn test_handshake_deadline() {
        // Stall the upgrade once its request headers arrive
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (release, pause) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            headers(&mut stream);
            pause.recv().unwrap();
        });

        // The attachment times out once the 100 ms pass
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
        // A request envelope encodes as a CBOR array and decodes back
        let encoded = Frame::Request(ID, vec![0xfb, 0xff]).encode();
        assert_eq!(hex::encode(&encoded), "86011b8000000000000007f642fbfff6f6");
        assert!(
            matches!(Frame::decode(&encoded).unwrap(), Frame::Request(ID, bytes) if bytes == [0xfb, 0xff])
        );

        // A foreign version, a missing or stray ID and two bodies are refused
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

        // So are trailing bytes and garbage
        let mut trailing = encoded;
        trailing.push(0);
        assert!(Frame::decode(&trailing).is_err());
        assert!(Frame::decode(&[0xff]).is_err());
    }
}
