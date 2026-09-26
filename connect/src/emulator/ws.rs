// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Ark connections over WebSocket, presented to wire as a blocking byte stream.
//!
//! One worker owns the WebSocket and handles binary data, Ping/Pong and Close.
//! Socket readiness and explicit wakeups drive its nonblocking I/O. The adapters
//! bound input buffering and wait for output under wire's write deadline.
//!
//! Wire's deadlines are measured on the connection's clock. The worker waits on
//! the socket through mio, so the bound on its own control replies runs on real time.

use crate::timing::ClockExt;
use crate::{Ark, Error, wire};
use darkbio_clock::{Clock, crossbeam_channel, sync};
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Bytes, HandshakeError, Message, WebSocket};
use wire::transport::{self, Verifier};

/// Readiness token of the socket owned by the worker.
const SOCKET: Token = Token(0);

/// Readiness token used when adapters queue output, consume input or close.
const WAKE: Token = Token(1);

/// Largest WebSocket message, including frame and recovery delimiters.
const MAX_MESSAGE: usize = transport::MAX_FRAME_SIZE + 2;

/// Maximum binary payload buffered ahead of the reader.
const INBOUND_LIMIT: usize = 2 * MAX_MESSAGE;

/// Opens a plain WebSocket endpoint and authenticates its wire session.
///
/// After address resolution, TCP establishment and HTTP upgrade share the
/// handshake timeout. The encrypted wire handshake starts its own timeout. The
/// connection measures its deadlines on the clock.
pub(crate) fn connect<V: Verifier<Info = crate::Identity>>(
    url: &str,
    verifier: &V,
    cloud: impl FnOnce(&crate::Identity) -> Option<(crate::trust::Environment, crate::trust::Realm)>,
    clock: &Clock,
) -> Result<(Ark, V::Info), Error> {
    // Resolve the endpoint before starting the TCP and HTTP handshake budget
    let request = url.into_client_request().map_err(Error::Upgrade)?;
    let uri = request.uri();
    if uri.scheme_str() != Some("ws") {
        return Err(Error::Unreachable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebSocket URL must use ws://",
        )));
    }
    let host = uri.host().ok_or_else(|| {
        Error::Unreachable(io::Error::new(io::ErrorKind::InvalidInput, "missing host"))
    })?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = (host, uri.port_u16().unwrap_or(80))
        .to_socket_addrs()
        .map_err(Error::Unreachable)?;

    // Failed address attempts consume the same budget as the eventual upgrade
    let deadline = clock.now() + transport::DEFAULT_HANDSHAKE_TIMEOUT;
    let mut last_error = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "host resolves to no address",
    );
    let mut connected = None;
    for address in addresses {
        let left = clock.remaining(deadline).map_err(Error::Unreachable)?;
        match TcpStream::connect_timeout(&address, left) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(err) => last_error = err,
        }
    }
    let tcp = connected.ok_or(Error::Unreachable(last_error))?;
    tcp.set_nodelay(true).map_err(Error::Unreachable)?;

    // Bound both protocol buffers before accepting peer WebSocket traffic. An
    // expired socket timeout typically reads as WouldBlock on Unix and as
    // TimedOut on Windows, so both end the upgrade as a timeout.
    let config = WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (socket, _) = tungstenite::client::client_with_config(
        request,
        Socket::Handshake {
            clock: clock.clone(),
            stream: tcp,
            deadline,
        },
        Some(config),
    )
    .map_err(|err| match err {
        HandshakeError::Interrupted(_) => Error::Timeout,
        HandshakeError::Failure(tungstenite::Error::Io(err))
            if err.kind() == io::ErrorKind::TimedOut =>
        {
            Error::Timeout
        }
        HandshakeError::Failure(err) => Error::Upgrade(err),
    })?;

    // Transfer the upgraded socket to its worker and give wire blocking adapters
    let (reader, writer, shutdown) = adapters(socket, clock).map_err(Error::Unreachable)?;
    Ark::attach(
        transport::Stream::new(reader, writer, shutdown),
        verifier,
        cloud,
    )
}

/// Socket used during the blocking HTTP upgrade and subsequent nonblocking I/O.
///
/// Replacing its adapter keeps the bytes already buffered by the WebSocket.
enum Socket {
    /// TCP stream whose individual I/O calls share the upgrade deadline.
    Handshake {
        /// Clock of the connection, which the deadline is measured on.
        clock: Clock,
        /// TCP connection being upgraded, before readiness registration.
        stream: TcpStream,
        /// Shared absolute bound for every HTTP upgrade read and write.
        deadline: Instant,
    },
    /// Registered socket whose I/O rearms readiness through mio.
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    /// Applies the upgrade deadline or delegates to the registered socket.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Handshake {
                clock,
                stream,
                deadline,
            } => {
                stream.set_read_timeout(Some(clock.remaining(*deadline)?))?;
                stream.read(buf)
            }
            Self::Connected(stream) => stream.read(buf),
        }
    }
}

impl Write for Socket {
    /// Applies the upgrade deadline or writes through readiness-aware I/O.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Handshake {
                clock,
                stream,
                deadline,
            } => {
                stream.set_write_timeout(Some(clock.remaining(*deadline)?))?;
                stream.write(buf)
            }
            Self::Connected(stream) => stream.write(buf),
        }
    }

    /// Flushes the underlying TCP adapter; WebSocket buffering lives above it.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Handshake { stream, .. } => stream.flush(),
            Self::Connected(stream) => stream.flush(),
        }
    }
}

/// Reports output refused after the connection or its worker has closed.
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "WebSocket connection closed")
}

/// Preserves socket I/O errors and wraps WebSocket protocol failures for wire.
fn socket_error(err: tungstenite::Error) -> io::Error {
    match err {
        tungstenite::Error::Io(err) => err,
        err => io::Error::other(err),
    }
}

/// Checks whether the WebSocket needs a readiness event before it can make
/// progress.
fn would_block(err: &tungstenite::Error) -> bool {
    matches!(err, tungstenite::Error::Io(err) if err.kind() == io::ErrorKind::WouldBlock)
}

/// Binary input waiting for the reader, followed by the worker's ending result.
#[derive(Default)]
struct Incoming {
    /// Binary messages kept until the reader consumes them.
    chunks: VecDeque<Bytes>,
    /// Payload bytes still charged against the input limit.
    bytes: usize,
    /// Flag set once the worker stops producing input.
    ended: bool,
    /// Failure that ended the worker, returned after the buffered input.
    error: Option<io::Error>,
}

/// Input and closure state shared by the worker and its blocking adapters.
struct Shared {
    /// Clock that wire's deadlines are measured on.
    clock: Clock,
    /// Local shutdown signal that every participant checks.
    closed: AtomicBool,
    /// Buffered input and the worker's ending result.
    incoming: sync::Mutex<Incoming>,
    /// Condition that wakes the reader for input, failure or closure.
    available: sync::Condvar,
    /// Waker of the worker, for when an adapter changes its work.
    wake: Waker,
    /// Channel notifying a test each time the flush of a writer's frame waits
    /// for the socket.
    #[cfg(test)]
    blocked: std::sync::Mutex<Option<mpsc::Sender<()>>>,
}

impl Shared {
    /// Marks local closure and wakes both the reader and the socket worker.
    fn close(&self) {
        // Pair the condition change with the reader's lock to prevent a lost wake
        let _incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        self.closed.store(true, Ordering::Release);
        self.available.notify_all();
        let _ = self.wake.wake();
    }

    /// Publishes the worker's ending result after any input already buffered.
    fn end(&self, error: Option<io::Error>) {
        let mut incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        incoming.ended = true;
        incoming.error = error;
        self.available.notify_all();
    }

    /// Checks that a complete message still fits before the worker reads
    /// another one.
    ///
    /// The chunk limit also bounds overhead from many small binary messages.
    fn can_read(&self) -> bool {
        let incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        incoming.bytes <= INBOUND_LIMIT - MAX_MESSAGE && incoming.chunks.len() < 1024
    }

    /// Queues a binary message and wakes the reader.
    ///
    /// An empty message is dropped, since it carries no bytes.
    fn push(&self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        let mut incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        incoming.bytes += bytes.len();
        incoming.chunks.push_back(bytes);
        self.available.notify_one();
    }

    /// Notifies a waiting test that the flush of a writer's frame waits for the socket.
    #[cfg(test)]
    fn flush_blocked(&self) {
        if let Some(sender) = self.blocked.lock().unwrap().as_ref() {
            let _ = sender.send(());
        }
    }
}

/// One flush submitted by the writer, acknowledged when the worker finishes it.
struct Outgoing {
    /// Complete frame accumulated since the previous flush.
    bytes: Vec<u8>,
    /// Original write deadline, which includes the wait in the queue.
    deadline: Instant,
    /// Sender of the local write result, which never blocks the socket worker.
    done: crossbeam_channel::Sender<io::Result<()>>,
}

/// Starts the socket worker and returns adapters with their shutdown operation.
///
/// The returned shutdown wakes blocking I/O and closes the underlying socket.
/// The adapters measure wire's deadlines on the clock.
fn adapters(
    mut socket: WebSocket<Socket>,
    clock: &Clock,
) -> io::Result<(Reader, Writer, impl FnOnce() + Send + use<>)> {
    // Switch the upgraded stream to nonblocking I/O registered with a poll
    let Socket::Handshake { stream, .. } = socket.get_ref() else {
        unreachable!("socket upgraded once")
    };
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    let mut connected = mio::net::TcpStream::from_std(stream.try_clone()?);
    let closing = stream.try_clone()?;
    let poll = Poll::new()?;
    poll.registry().register(
        &mut connected,
        SOCKET,
        Interest::READABLE | Interest::WRITABLE,
    )?;
    // Reads and writes must use mio's socket, which rearms readiness on Windows
    *socket.get_mut() = Socket::Connected(connected);

    // Run the socket worker, handing its adapters to wire
    let shared = Arc::new(Shared {
        clock: clock.clone(),
        closed: AtomicBool::new(false),
        incoming: sync::Mutex::new(Incoming::default()),
        available: sync::Condvar::new(clock),
        wake: Waker::new(poll.registry(), WAKE)?,
        #[cfg(test)]
        blocked: std::sync::Mutex::new(None),
    });
    let (outgoing, outbox) = mpsc::channel();
    thread::Builder::new().name("ark-websocket".into()).spawn({
        let shared = shared.clone();
        move || pump(socket, poll, shared, outbox)
    })?;
    Ok((
        Reader {
            shared: shared.clone(),
            deadline: None,
        },
        Writer {
            shared: shared.clone(),
            outgoing,
            pending: Vec::new(),
            deadline: None,
        },
        move || {
            shared.close();
            let _ = closing.shutdown(Shutdown::Both);
        },
    ))
}

/// Bound on output the socket has not taken yet, the worker's own control
/// replies included.
///
/// A deferred flush starts the bound, later ones keep its deadline, and only a
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
        self.deadline
            .get_or_insert(now + transport::DEFAULT_WRITE_TIMEOUT);
    }

    /// Ends the bound once a flush took all output.
    fn flushed(&mut self) {
        self.deadline = None;
    }

    /// Checks whether the output left to flush missed its deadline by `now`.
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Returns the time left after `now` before the deadline, bounding the
    /// next poll.
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }
}

/// Drives the WebSocket while preserving input progress during blocked output.
///
/// One adapter writer submits flushes serially and waits for each
/// acknowledgement. Frame deadlines are wire's and measured on the connection's
/// clock; control replies are bounded on real time.
#[expect(
    clippy::disallowed_methods,
    reason = "the worker waits on the socket through mio, so the bound on its own control replies runs on real time"
)]
fn pump(
    mut socket: WebSocket<Socket>,
    mut poll: Poll,
    shared: Arc<Shared>,
    outbox: mpsc::Receiver<Outgoing>,
) {
    let clock = &shared.clock;
    let mut events = Events::with_capacity(8);
    let mut active: Option<Outgoing> = None;
    let mut backlog = Backlog::default();
    let mut peer_closed = false;
    let result = (|| -> io::Result<()> {
        loop {
            // Stop on local closure, and fail once a frame or the backlog runs
            // out of time
            if shared.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(frame) = &active {
                clock.remaining(frame.deadline)?;
            }
            if backlog.expired(Instant::now()) {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }

            // Admit at most one flush. Its bytes stay in the WebSocket until
            // output completes, while the original deadline continues to run.
            if active.is_none() && !peer_closed {
                match outbox.try_recv() {
                    Ok(mut frame) => {
                        if clock.remaining(frame.deadline).is_err() {
                            let _ = frame
                                .done
                                .send(Err(io::Error::from(io::ErrorKind::TimedOut)));
                            continue;
                        }
                        let message =
                            Message::Binary(Bytes::from(std::mem::take(&mut frame.bytes)));
                        active = Some(frame);
                        match socket.write(message) {
                            Ok(()) => {}
                            Err(err) if would_block(&err) => {}
                            Err(err) => return Err(socket_error(err)),
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }

            // Limit each batch so continuous ingress cannot starve sends or
            // deadlines
            let mut batch_full = false;
            for index in 0..32 {
                if peer_closed || !shared.can_read() {
                    break;
                }
                match socket.read() {
                    Ok(Message::Binary(bytes)) => shared.push(bytes),
                    Ok(Message::Close(_)) => {
                        peer_closed = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(err) if would_block(&err) => break,
                    Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
                    Err(err) => return Err(socket_error(err)),
                }
                batch_full = index == 31;
            }

            // Flush data and automatic Pong/Close replies through the same owner.
            // An acknowledgement covers everything queued before this flush.
            match socket.flush() {
                Ok(()) => {
                    backlog.flushed();
                    if let Some(frame) = active.take() {
                        let _ = frame.done.send(Ok(()));
                    }
                    if peer_closed {
                        return Ok(());
                    }
                }
                Err(err) if would_block(&err) => {
                    #[cfg(test)]
                    if active.is_some() {
                        shared.flush_blocked();
                    }
                    backlog.start(Instant::now());
                }
                Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
                Err(err) => return Err(socket_error(err)),
            }
            if batch_full {
                continue;
            }

            // Either socket readiness or an adapter wake may enable more work.
            // Deadlines must also wake an otherwise idle or blocked connection.
            let timeout = active
                .as_ref()
                .map(|frame| frame.deadline.saturating_duration_since(clock.now()))
                .into_iter()
                .chain(backlog.remaining(Instant::now()))
                .min();
            match poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    })();

    // Settle the outstanding flush before waking the reader with the ending
    // result
    if let Some(frame) = active {
        let error = result
            .as_ref()
            .err()
            .map_or_else(closed, |err| io::Error::new(err.kind(), err.to_string()));
        let _ = frame.done.send(Err(error));
    }
    shared.end(result.err());
    if let Socket::Connected(stream) = socket.get_ref() {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Blocking reader over buffered binary input from the socket worker.
struct Reader {
    /// Input queue and closure notifications shared with the worker.
    shared: Arc<Shared>,
    /// Deadline bounding the next wait for input.
    deadline: Option<Instant>,
}

impl Read for Reader {
    /// Drains buffered binary bytes before reporting closure or the worker
    /// error.
    ///
    /// Consuming input wakes the worker to resume reads after backpressure.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut incoming = self
            .shared
            .incoming
            .lock()
            .expect("WebSocket input not poisoned");
        loop {
            if let Some(front) = incoming.chunks.front_mut() {
                let count = buf.len().min(front.len());
                buf[..count].copy_from_slice(&front.split_to(count));
                if front.is_empty() {
                    incoming.chunks.pop_front();
                }
                incoming.bytes -= count;
                drop(incoming);
                let _ = self.shared.wake.wake();
                return Ok(count);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            if incoming.ended {
                return incoming.error.take().map_or(Ok(0), Err);
            }
            incoming = match self.deadline {
                None => self
                    .shared
                    .available
                    .wait(incoming)
                    .expect("WebSocket input not poisoned"),
                Some(deadline) => {
                    self.shared.clock.remaining(deadline)?;
                    self.shared
                        .available
                        .wait_deadline(incoming, deadline)
                        .expect("WebSocket input not poisoned")
                        .0
                }
            };
        }
    }
}

impl transport::Read for Reader {
    /// Returns the connection's clock, which the read deadlines are measured on.
    fn clock(&self) -> Clock {
        self.shared.clock.clone()
    }

    /// Bounds future input waits, leaving already buffered bytes available.
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.deadline = deadline;
        Ok(())
    }
}

/// Blocking writer whose flush waits for the socket worker's completion result.
struct Writer {
    /// Closure signal and worker wakeup shared with the worker.
    shared: Arc<Shared>,
    /// Queue of frames waiting for the socket worker.
    outgoing: mpsc::Sender<Outgoing>,
    /// Bytes accumulated since the last flush.
    pending: Vec<u8>,
    /// Deadline shared by the writes of a frame and their flush.
    deadline: Option<Instant>,
}

impl Write for Writer {
    /// Accumulates a frame locally; [`Self::flush`] submits it to the worker.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if let Some(deadline) = self.deadline {
            self.shared.clock.remaining(deadline)?;
        }
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Submits accumulated bytes and waits for local socket completion under the
    /// original write deadline.
    ///
    /// Success does not acknowledge receipt by the Ark.
    fn flush(&mut self) -> io::Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        let clock = &self.shared.clock;
        let deadline = self
            .deadline
            .unwrap_or_else(|| clock.now() + transport::DEFAULT_WRITE_TIMEOUT);
        clock.remaining(deadline)?;
        if self.pending.is_empty() {
            return Ok(());
        }

        // Hand the frame to the worker and wait for its local write result
        let (done, result) = crossbeam_channel::bounded(1);
        self.outgoing
            .send(Outgoing {
                bytes: std::mem::take(&mut self.pending),
                deadline,
                done,
            })
            .map_err(|_| closed())?;
        self.shared.wake.wake()?;
        clock.remaining(deadline)?;
        match clock.recv_deadline(&result, deadline) {
            Ok(result) => result,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                Err(io::Error::from(io::ErrorKind::TimedOut))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }
}

impl transport::Write for Writer {
    /// Returns the connection's clock, which the write deadline is measured on.
    fn clock(&self) -> Clock {
        self.shared.clock.clone()
    }

    /// Installs the shared bound for accumulating and flushing the next frame.
    fn set_write_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        self.deadline = Some(deadline);
        Ok(())
    }
}

/// WebSocket control, buffering, deadline and wire session regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Peer, answering, hangup, test_clock, wait_deadline};
    use darkbio_wire::memory::Duplex;
    use std::net::TcpListener;
    use std::thread;
    use tungstenite::protocol::Role;

    /// Connects a client socket to a local WebSocket server, for exercising the
    /// adapters without a wire session consuming their bytes.
    ///
    /// The upgrade's socket timeouts are measured from the clock, which the
    /// tests never advance through them.
    fn socket_pair(clock: &Clock) -> (WebSocket<Socket>, WebSocket<TcpStream>) {
        // Serve one WebSocket upgrade on a local listener
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            tcp.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            tungstenite::accept(tcp).unwrap()
        });

        // Upgrade the client over the socket type the adapters take over
        let tcp = TcpStream::connect(addr).unwrap();
        let (client, _) = tungstenite::client(
            format!("ws://{addr}/v1/usb"),
            Socket::Handshake {
                clock: clock.clone(),
                stream: tcp,
                deadline: clock.now() + Duration::from_secs(2),
            },
        )
        .unwrap();
        (client, server.join().unwrap())
    }

    /// Ping and Close receive protocol replies while the application is idle.
    #[test]
    fn test_control_frames() {
        // A ping gets its pong while the application reads nothing
        let clock = test_clock().clock();
        let (socket, mut server) = socket_pair(&clock);
        let (mut reader, _writer, shutdown) = adapters(socket, &clock).unwrap();
        server
            .send(Message::Ping(Bytes::from_static(b"probe")))
            .unwrap();
        assert_eq!(
            server.read().unwrap(),
            Message::Pong(Bytes::from_static(b"probe"))
        );

        // A close gets its reply and ends the reader's stream
        server.close(None).unwrap();
        assert!(matches!(server.read().unwrap(), Message::Close(_)));
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        shutdown();
    }

    /// A read respects its deadline, and shutdown also wakes a read without one.
    #[test]
    fn test_read_deadline_and_close() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (socket, _server) = socket_pair(&clock);
        let (mut reader, _writer, shutdown) = adapters(socket, &clock).unwrap();

        // The read waits on its deadline and ends once the clock reaches it
        let deadline = clock.now() + Duration::from_millis(20);
        transport::Read::set_read_deadline(&mut reader, Some(deadline)).unwrap();
        let reading = thread::spawn(move || {
            let result = reader.read(&mut [0]);
            (reader, result)
        });
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        let (mut reader, result) = reading.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);

        // Shutdown wakes a read that waits without a deadline. The reader is the
        // only thread that waits on this clock, so the park after its start is
        // the read's own. No deadline is listed for that park.
        transport::Read::set_read_deadline(&mut reader, None).unwrap();
        let (started, reading) = mpsc::channel();
        let read = thread::spawn(move || {
            started.send(()).unwrap();
            reader.read(&mut [0])
        });
        reading.recv().unwrap();
        tester.wait_blocked(1);
        assert_eq!(tester.next_deadline(), None);
        shutdown();
        assert_eq!(read.join().unwrap().unwrap(), 0);
    }

    /// Consuming buffered input releases capacity for the remaining messages.
    #[test]
    fn test_input_buffering() {
        // The peer sends four of the largest messages, twice what the input
        // limit holds
        let clock = test_clock().clock();
        let (socket, mut server) = socket_pair(&clock);
        let (mut reader, _writer, shutdown) = adapters(socket, &clock).unwrap();
        let sending = thread::spawn(move || {
            let message = Bytes::from(vec![7; MAX_MESSAGE]);
            for _ in 0..4 {
                server.send(Message::Binary(message.clone())).unwrap();
            }
            server
        });

        // Reading each message frees room for the rest, never passing the limit
        let mut message = vec![0; MAX_MESSAGE];
        for _ in 0..4 {
            reader.read_exact(&mut message).unwrap();
            assert!(message.iter().all(|byte| *byte == 7));
            assert!(reader.shared.incoming.lock().unwrap().bytes <= INBOUND_LIMIT);
        }
        let _server = sending.join().unwrap();
        shutdown();
    }

    /// Blocked output keeps its deadline while incoming traffic still reaches
    /// the reader.
    #[test]
    fn test_output_backpressure() {
        // Flood the peer with output it never drains, under one write deadline,
        // until the worker's flush of a frame waits for the socket
        let mut tester = test_clock();
        let clock = tester.clock();
        let (socket, mut server) = socket_pair(&clock);
        let (_reader, mut writer, shutdown) = adapters(socket, &clock).unwrap();
        let shared = writer.shared.clone();
        let (blocked, flushes) = mpsc::channel();
        *shared.blocked.lock().unwrap() = Some(blocked);
        let deadline = clock.now() + Duration::from_millis(300);
        transport::Write::set_write_deadline(&mut writer, deadline).unwrap();
        let sending = thread::spawn(move || -> io::Result<()> {
            let message = vec![9; MAX_MESSAGE];
            loop {
                writer.write_all(&message)?;
                writer.flush()?;
            }
        });
        flushes.recv().unwrap();

        // The peer sends input, which reaches the reader past the stuck output
        server
            .send(Message::Binary(Bytes::from_static(b"incoming")))
            .unwrap();
        let incoming = shared.incoming.lock().unwrap();
        let incoming = shared
            .available
            .wait_while(incoming, |incoming| incoming.bytes == 0 && !incoming.ended)
            .unwrap();
        assert_eq!(incoming.chunks.front().unwrap().as_ref(), b"incoming");
        drop(incoming);

        // The writer waits for the stuck flush and gives up at its deadline
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        assert_eq!(
            sending.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        shutdown();
    }

    /// A blocked flush starts the output bound once, later ones keep its
    /// deadline, and a completed flush clears it for the next blockage.
    #[test]
    fn test_backlog() {
        // The first blocked flush starts the bound
        let mut tester = test_clock();
        let clock = tester.clock();
        let limit = transport::DEFAULT_WRITE_TIMEOUT;
        let mut backlog = Backlog::default();
        assert_eq!(backlog.remaining(clock.now()), None);
        let start = clock.now();
        backlog.start(start);
        assert_eq!(backlog.remaining(start), Some(limit));

        // Blocking again keeps the original deadline, which ends the bound on time
        tester.advance(Duration::from_secs(3));
        backlog.start(clock.now());
        assert_eq!(
            backlog.remaining(clock.now()),
            Some(limit - Duration::from_secs(3))
        );
        tester.advance_to(start + limit - Duration::from_millis(1));
        assert!(!backlog.expired(clock.now()));
        tester.advance_to(start + limit);
        assert!(backlog.expired(clock.now()));

        // A completed flush clears the bound, and the next blockage starts afresh
        backlog.flushed();
        assert!(!backlog.expired(clock.now()));
        assert_eq!(backlog.remaining(clock.now()), None);
        backlog.start(clock.now());
        assert_eq!(backlog.remaining(clock.now()), Some(limit));
    }

    /// Bridges one WebSocket client on the listener to the peer's stream,
    /// standing in for an emulator.
    ///
    /// Binary messages carry the bytes both ways until either side ends. Each
    /// direction has its own thread and socket handle and blocks on its own
    /// input.
    fn bridge(listener: TcpListener, stream: Duplex) {
        // Split the peer's stream and accept the client's upgrade
        let (mut reader, mut writer) = stream.into_halves();
        let (tcp, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();

        // Probe the client with a ping, then carry the peer's output to it,
        // ending the connection once the peer is gone
        let mut outgoing =
            WebSocket::from_raw_socket(socket.get_ref().try_clone().unwrap(), Role::Server, None);
        outgoing
            .send(Message::Ping(Bytes::from_static(b"keepalive")))
            .unwrap();
        let sending = thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(count @ 1..) = reader.read(&mut buf) {
                let message = Message::Binary(Bytes::copy_from_slice(&buf[..count]));
                if outgoing.send(message).is_err() {
                    break;
                }
            }
            let _ = outgoing.get_ref().shutdown(Shutdown::Both);
        });

        // Carry the client's messages into the peer, until the client goes away
        let mut pong_received = false;
        loop {
            match socket.read() {
                Ok(Message::Binary(data)) => {
                    if writer
                        .write_all(&data)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Pong(bytes)) => {
                    assert_eq!(bytes.as_ref(), b"keepalive");
                    pong_received = true;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }

        // Close the peer's input, then check that the client answered the probe
        drop(writer);
        sending.join().unwrap();
        assert!(
            pong_received,
            "client must answer Ping while carrying wire traffic"
        );
    }

    /// A session over an emulator's socket reaches the peer behind it, and
    /// closing the session ends the socket.
    #[test]
    fn test_socket_session() {
        // Bridge a local WebSocket listener to an answering peer
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, Box::new(answering));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/usb", listener.local_addr().unwrap());
        let stream = peer.stream();
        let served = thread::spawn(move || bridge(listener, stream));

        // A request crosses the socket, and closing the session ends the bridge
        let (ark, _) = connect(
            &url,
            &crate::TrustMode::Recover(Box::new(peer.identity.clone())),
            |_| None,
            &clock,
        )
        .unwrap();
        assert_eq!(
            ark.client()
                .call_timeout(crate::schema::DeviceInfoRequest {}, Duration::from_secs(2))
                .unwrap()
                .firmware_version,
            "1.0.0"
        );
        ark.close();
        served.join().unwrap();
    }

    /// The emulator going away ends the session as a disconnect once the socket
    /// behind it is gone.
    #[test]
    fn test_socket_lost() {
        // Bridge a local WebSocket listener to a peer that hangs up
        let clock = test_clock().clock();
        let mut peer = Peer::spawn(&clock, hangup());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/usb", listener.local_addr().unwrap());
        let stream = peer.stream();
        let served = thread::spawn(move || bridge(listener, stream));

        // The request fails as a disconnect, and the bridge ends
        let (ark, _) = connect(
            &url,
            &crate::TrustMode::Recover(Box::new(peer.identity.clone())),
            |_| None,
            &clock,
        )
        .unwrap();
        let err = ark
            .client()
            .call_timeout(crate::schema::DeviceInfoRequest {}, Duration::from_secs(2))
            .unwrap_err();
        assert!(matches!(err, Error::Disconnected(_)), "{err:?}");
        served.join().unwrap();
    }
}
