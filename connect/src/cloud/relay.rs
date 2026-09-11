// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Carries opaque companion messages between the cloud socket and the Ark.

use super::{Failure, dns};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use darkbio_crypto::cbor::{self, Cbor};
use darkbio_wire::protocol::{self, Promise, Requester, Responder, schema};
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Relay bodies fit inside a wire message. Queues and requests also have limits.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_INFLIGHT: usize = 128;

/// The firmware bounds its relay requests by sixty seconds. This also bounds
/// retained responders when a companion never answers, independently of callers.
pub(super) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET: Token = Token(0);
const WAKE: Token = Token(1);

/// An attached relay. Dropping it ends its worker and outstanding forwarding.
#[derive(Debug)]
pub(super) struct Relay {
    shared: Arc<Shared>,    // Queue and ending reason observed by the dispatcher
    worker: Option<Worker>, // Started when dispatch can see this attachment
}

#[derive(Debug)]
struct Worker {
    socket: WebSocket<MaybeTlsStream<Socket>>,
    poll: Poll,
    requester: Requester,
    heartbeat: Heartbeat,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>, // Admission and closure are atomic with respect to each other
    wake: Waker,         // Signals queued Ark traffic or local closure
    socket: TcpStream,   // Interrupts reads even inside WebSocket message assembly
}

#[derive(Debug, Default)]
struct State {
    queue: VecDeque<(schema::RelayArkToAppRequest, Responder, Instant)>,
    bytes: usize,          // Opaque bytes waiting for the socket worker
    error: Option<String>, // First reason this relay ended
}

impl Relay {
    /// Opens an authenticated socket under the original operation's deadline.
    pub(super) fn connect(
        url: &str,
        auth: &[u8],
        requester: Requester,
        deadline: Instant,
    ) -> Result<Self, Failure> {
        let mut request = url.into_client_request().map_err(socket_error)?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!(
                "Relaying, Dark-Auth|{}",
                BASE64_URL_SAFE_NO_PAD.encode(auth)
            )
            .parse()
            .expect("base64url is a valid header"),
        );
        let host = request
            .uri()
            .host()
            .ok_or_else(|| Failure::Relay("relay URL has no host".into()))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port =
            request
                .uri()
                .port_u16()
                .unwrap_or(if request.uri().scheme_str() == Some("wss") {
                    443
                } else {
                    80
                });

        let addresses = dns::resolve(&host, port, deadline)?;
        let mut failure = io::Error::new(io::ErrorKind::AddrNotAvailable, "relay has no address");
        let mut connected = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, remaining(deadline).map_err(io_error)?) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => failure = error,
            }
        }
        let stream = connected.ok_or_else(|| io_error(failure))?;
        stream.set_nodelay(true).map_err(io_error)?;
        let config = WebSocketConfig::default()
            .write_buffer_size(0)
            .max_write_buffer_size(2 * MAX_MESSAGE)
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let (mut socket, response) = tungstenite::client_tls_with_config(
            request,
            Socket::Handshake { stream, deadline },
            Some(config),
            None,
        )
        .map_err(|error| match error {
            HandshakeError::Interrupted(_) => Failure::Wire(protocol::Error::Timeout),
            HandshakeError::Failure(error) => socket_error(error),
        })?;
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok())
            != Some("Relaying")
        {
            return Err(Failure::Relay(
                "cloud did not select the Relaying subprotocol".into(),
            ));
        }
        remaining(deadline).map_err(io_error)?;
        let Socket::Handshake { stream, .. } = socket_mut(&mut socket) else {
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
            wake: Waker::new(poll.registry(), WAKE).map_err(io_error)?,
            socket: shutdown,
        });
        Ok(Self {
            shared,
            worker: Some(Worker {
                socket,
                poll,
                requester,
                heartbeat: Heartbeat::new(PING_INTERVAL, PONG_TIMEOUT),
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

    /// Whether this particular attachment has ended. A new call may replace it.
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

    pub(super) fn close(&self) {
        self.shared.end("relay closed".into());
    }
}

impl Drop for Relay {
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

/// The socket's blocking handshake charges every read and write to one deadline.
#[derive(Debug)]
enum Socket {
    Handshake {
        stream: TcpStream,
        deadline: Instant,
    },
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Handshake { stream, deadline } => {
                stream.set_read_timeout(Some(remaining(*deadline)?))?;
                stream.read(bytes)
            }
            Self::Connected(stream) => stream.read(bytes),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Handshake { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.write(bytes)
            }
            Self::Connected(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Handshake { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.flush()
            }
            Self::Connected(stream) => stream.flush(),
        }
    }
}

/// Accesses the readiness adapter under either the cleartext test socket or TLS.
fn socket_mut(socket: &mut WebSocket<MaybeTlsStream<Socket>>) -> &mut Socket {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &mut stream.sock,
        _ => unreachable!("only plain and rustls sockets are enabled"),
    }
}

pub(super) fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

pub(super) fn io_error(error: io::Error) -> Failure {
    match error.kind() {
        io::ErrorKind::TimedOut => Failure::Wire(protocol::Error::Timeout),
        _ => Failure::Relay(error.to_string()),
    }
}

fn socket_error(error: tungstenite::Error) -> Failure {
    match error {
        tungstenite::Error::Io(error) => io_error(error),
        error => Failure::Relay(error.to_string()),
    }
}

fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

/// An unavailable relay fails the Ark's reverse request, allowing its original
/// operation to finish with an error. Enqueueing the reply does not wait on I/O.
pub(super) fn fail(responder: Responder, reason: &str) {
    let _ = responder.fail(
        schema::Error::reserved(schema::ReservedErrors::Unavailable, reason),
        Instant::now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT,
    );
}

/// Current cloud envelope. Only the envelope is interpreted; all bodies stay sealed.
#[derive(Cbor, Default)]
#[cbor(array)]
struct Envelope {
    darkrpc: u64,
    id: Option<u64>,
    notify: Option<Vec<u8>>,
    request: Option<Vec<u8>>,
    response: Option<Vec<u8>>,
    presence: Option<Vec<u8>>,
}

enum Frame {
    Request(u64, Vec<u8>),
    Response(u64, Vec<u8>),
    Notice,
}

impl Frame {
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
    interval: Duration,
    timeout: Duration,
    next: Instant,
    sequence: u64,
    pending: Option<(u64, Instant)>,
}

impl Heartbeat {
    fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            next: Instant::now() + interval,
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

    fn pong(&mut self, bytes: &[u8], now: Instant) {
        if let Some((sequence, deadline)) = self.pending
            && now < deadline
            && bytes == sequence.to_be_bytes()
        {
            self.pending = None;
            self.next = now + self.interval;
        }
    }

    fn deadline(&self) -> Instant {
        self.pending.map_or(self.next, |(_, deadline)| deadline)
    }
}

/// Writes one frame at a time while receiving concurrently. Wire completions
/// are polled only while app requests are in flight; idle wakeups check liveness.
fn pump(
    mut socket: WebSocket<MaybeTlsStream<Socket>>,
    mut poll: Poll,
    requester: Requester,
    shared: Arc<Shared>,
    mut heartbeat: Heartbeat,
) {
    let mut events = Events::with_capacity(8);
    let mut pending: HashMap<u64, (Responder, Instant)> = HashMap::new();
    let mut inbound: HashMap<u64, Promise<protocol::Message>> = HashMap::new();
    let (answered, answers) = mpsc::channel();
    let mut output = VecDeque::new();
    let mut bytes = 0usize;
    let mut writing = None;
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
                    && writing.is_none()
                    && pending.len() < MAX_INFLIGHT
                    && let Some((request, responder, deadline)) = state.queue.pop_front()
                {
                    state.bytes -= request.req.len();
                    if deadline <= Instant::now() {
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
            let expired: Vec<_> = pending
                .iter()
                .filter(|(_, (_, deadline))| Instant::now() >= *deadline)
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
            if writing.is_none()
                && let Some(frame) = output.pop_front()
            {
                bytes -= frame.len();
                writing = Some(Instant::now() + WRITE_TIMEOUT);
                match socket.write(Message::Binary(frame.into())) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(socket_error(error)),
                }
            }
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
                                Instant::now() + EXCHANGE_TIMEOUT,
                            )?;
                            promise.notify(answered.clone(), id);
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
                writing.get_or_insert_with(|| Instant::now() + WRITE_TIMEOUT);
                match socket.write(Message::Ping(ping.to_vec().into())) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(socket_error(error)),
                }
            }
            match socket.flush() {
                Ok(()) => writing = None,
                Err(error) if would_block(&error) => {
                    writing.get_or_insert_with(|| Instant::now() + WRITE_TIMEOUT);
                }
                Err(error) => return Err(socket_error(error)),
            }
            if let Some(deadline) = writing {
                remaining(deadline).map_err(io_error)?;
            }
            let queued = !shared
                .state
                .lock()
                .expect("relay queue not poisoned")
                .queue
                .is_empty();
            if batch_full
                || (writing.is_none()
                    && (!output.is_empty() || (queued && pending.len() < MAX_INFLIGHT)))
            {
                continue;
            }
            let deadline = pending
                .values()
                .map(|(_, deadline)| *deadline)
                .chain(writing)
                .chain(Some(heartbeat.deadline()))
                .chain((!inbound.is_empty()).then(|| Instant::now() + Duration::from_millis(10)))
                .min();
            let timeout =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
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
    use crate::testing::{Peer, answering};
    use crate::{Error, schema::host_to_ark::Content};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tungstenite::handshake::server::{Request, Response};

    /// IDs retain all sixty-four bits, including numbers beyond JSON precision.
    const ID: u64 = (1 << 63) + 7;

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "test cloud was never contacted");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("test accept: {error}"),
            }
        }
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
        listener.set_nonblocking(true).unwrap();
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

    /// Keeps serving app requests while an unlock waits for its reverse response.
    fn peer() -> (Peer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let joins = Arc::new(AtomicUsize::new(0));
        let syncs = Arc::new(AtomicUsize::new(0));
        let peer = Peer::spawn(Box::new({
            let joins = joins.clone();
            let syncs = syncs.clone();
            let mut synced = false;
            let mut next_id = ID;
            move |session, request, responder| {
                let deadline = Instant::now() + TIMEOUT;
                match request {
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
                    request @ (Content::Unlock(_)
                    | Content::ExecSched(_)
                    | Content::SlotRepair(_)
                    | Content::SlotDelete(_)
                    | Content::FirmwareUpdatePrep(_)
                    | Content::SlotUploadStart(_)) => {
                        let (reply, preflight, authorize): (protocol::Message, _, _) = match request
                        {
                            Content::Unlock(_) => (schema::UnlockResponse {}.into(), true, true),
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
        }));
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
            pause.recv_timeout(TIMEOUT).unwrap();
        });
        let (mut peer, joins, syncs) = peer();
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
        assert_eq!(joins.load(Ordering::SeqCst), 0);
        assert_eq!(syncs.load(Ordering::SeqCst), 0);
        client.call(schema::UnlockRequest {}, deadline).unwrap();
        stages.recv_timeout(TIMEOUT).unwrap();
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
            pause.recv_timeout(TIMEOUT).unwrap();
        });
        let (mut peer, joins, _) = peer();
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
        let calls: [Call; 3] = [
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
        ];
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
                pause.recv_timeout(TIMEOUT).unwrap();
            });
            let (mut peer, joins, _) = peer();
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
                pause.recv_timeout(TIMEOUT).unwrap();
            });
            let (mut peer, joins, _) = peer();
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
        let (seen, attempts) = mpsc::channel();
        let (release, pause) = mpsc::channel();
        let (finish, done) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            seen.send(()).unwrap();
            pause.recv_timeout(TIMEOUT).unwrap();
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
            done.recv_timeout(TIMEOUT).unwrap();
        });
        let (mut peer, joins, syncs) = peer();
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let deadline = Instant::now() + TIMEOUT;
        let leader = thread::spawn({
            let client = client.clone();
            move || client.call(schema::UnlockRequest {}, deadline)
        });
        attempts.recv_timeout(TIMEOUT).unwrap();
        client.call(schema::DeviceInfoRequest {}, deadline).unwrap();
        assert!(matches!(
            client
                .clone()
                .call_timeout(schema::UnlockRequest {}, Duration::from_millis(20)),
            Err(Error::Timeout)
        ));
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
        let (release, pause) = mpsc::channel();
        let (url, server) = cloud(1, move |_, stream| {
            let mut socket = upgrade(stream);
            assert!(matches!(frame(&mut socket), Frame::Request(_, _)));
            pause.recv_timeout(TIMEOUT).unwrap();
        });
        let (mut peer, _, _) = peer();
        let ark = attach(&mut peer, url);
        let client = ark.client();
        assert!(matches!(
            client.call_timeout(schema::UnlockRequest {}, Duration::from_millis(500)),
            Err(Error::Timeout)
        ));
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
                    pause.recv_timeout(TIMEOUT).unwrap();
                }
            });
            let (mut peer, joins, syncs) = peer();
            let ark = attach(&mut peer, url);
            let client = ark.client();
            let deadline = Instant::now() + TIMEOUT;
            let error = client.call(schema::UnlockRequest {}, deadline).unwrap_err();
            if refused {
                assert!(matches!(error, Error::Relay(_)));
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
        let (mut peer, _, _) = peer();
        let ark = attach(&mut peer, url);
        let client = ark.client();
        let pending = client
            .send(schema::UnlockRequest {}, Instant::now() + TIMEOUT)
            .unwrap();
        requests.recv_timeout(TIMEOUT).unwrap();
        drop(ark);
        assert!(matches!(pending.wait(), Err(Error::Closed)));
        assert!(matches!(
            client.call(schema::UnlockRequest {}, Instant::now() + TIMEOUT),
            Err(Error::Closed)
        ));
        server.join().unwrap();
    }

    /// Unsolicited or late pongs cannot acknowledge a different probe or restart
    /// its deadline. A valid pong schedules a fresh probe with a different ID.
    #[test]
    fn test_heartbeat() {
        let mut heartbeat = Heartbeat::new(PING_INTERVAL, PONG_TIMEOUT);
        let now = heartbeat.deadline();
        let first = heartbeat.ping(now).unwrap().unwrap();
        let deadline = heartbeat.deadline();
        assert!(heartbeat.ping(now).unwrap().is_none());
        heartbeat.pong(&[0; 8], now);
        assert_eq!(heartbeat.deadline(), deadline);
        heartbeat.pong(&first, now);
        assert_eq!(heartbeat.deadline(), now + PING_INTERVAL);
        let second = heartbeat.ping(heartbeat.deadline()).unwrap().unwrap();
        assert_ne!(first, second);
        let deadline = heartbeat.deadline();
        heartbeat.pong(&first, deadline - Duration::from_millis(1));
        heartbeat.pong(&second, deadline);
        assert!(heartbeat.ping(deadline).is_err());
    }

    /// The socket pump accepts matching pongs, then ends an unresponsive relay
    /// without disrupting local wire calls. A replacement can attach afterward.
    #[test]
    fn test_heartbeat_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (release, pause) = mpsc::channel();
        let (seen, pings) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            for _ in 0..3 {
                assert!(matches!(socket.read().unwrap(), Message::Ping(_)));
                socket.flush().unwrap();
            }
            assert!(matches!(socket.read().unwrap(), Message::Ping(_)));
            // Replace tungstenite's automatic pong with one that does not match.
            socket.send(Message::Pong(vec![0].into())).unwrap();
            seen.send(()).unwrap();
            assert!(matches!(socket.read(), Err(_) | Ok(Message::Close(_))));
            let _replacement = upgrade(accept(&listener));
            pause.recv_timeout(TIMEOUT).unwrap();
        });
        let mut peer = Peer::spawn(Box::new(answering));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut relay = Relay::connect(&url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
        relay.worker.as_mut().unwrap().heartbeat =
            Heartbeat::new(Duration::from_millis(20), Duration::from_millis(200));
        relay.start().unwrap();
        pings.recv_timeout(TIMEOUT).unwrap();
        while relay.connected() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
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
        let mut replacement =
            Relay::connect(&url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
        replacement.start().unwrap();
        assert!(replacement.connected());
        release.send(()).unwrap();
        server.join().unwrap();
    }

    /// Closing the attachment shuts down TCP even before its worker starts or
    /// while it is assembling an unfinished fragmented message.
    #[test]
    fn test_close_fragmented_message() {
        for started in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
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
            let mut peer = Peer::spawn(Box::new(answering));
            let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let deadline = Instant::now() + TIMEOUT;
            let mut relay =
                Relay::connect(&url, &[0xfb, 0xff], session.requester(), deadline).unwrap();
            if started {
                relay.start().unwrap();
            }
            fragments.recv_timeout(TIMEOUT).unwrap();
            relay.close();
            server.join().unwrap();
            while Arc::strong_count(&relay.shared) != 1 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                Arc::strong_count(&relay.shared),
                1,
                "relay worker must exit"
            );
        }
    }

    /// Every read of a stalled upgrade retains the caller's original deadline.
    #[test]
    fn test_handshake_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}/v1/relaying", listener.local_addr().unwrap());
        let (release, pause) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            headers(&mut stream);
            pause.recv_timeout(TIMEOUT).unwrap();
        });
        let mut peer = Peer::spawn(Box::new(answering));
        let verifier = crate::TrustMode::Recover(Box::new(peer.identity.clone()));
        let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
        let result = Relay::connect(
            &url,
            &[1],
            session.requester(),
            Instant::now() + Duration::from_millis(100),
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
