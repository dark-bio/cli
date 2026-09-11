// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Ark connections over WebSocket, presented to wire as a blocking byte stream.
//!
//! One worker owns the WebSocket and handles binary data, Ping/Pong and Close.
//! Socket readiness and explicit wakeups drive its nonblocking I/O. The adapters
//! bound input buffering and wait for output under wire's write deadline.

use crate::{Ark, Error, wire};
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
/// After address resolution, TCP establishment and HTTP upgrade share the
/// handshake timeout. The encrypted wire handshake starts its own timeout.
pub(crate) fn connect<V: Verifier<Info = crate::Identity>>(
    url: &str,
    verifier: &V,
) -> Result<(Ark, V::Info), Error> {
    // Resolve the endpoint before starting the TCP and HTTP handshake budget.
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
    // Failed address attempts consume the same budget as the eventual upgrade.
    let deadline = Instant::now() + transport::DEFAULT_HANDSHAKE_TIMEOUT;
    let mut last_error = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "host resolves to no address",
    );
    let mut connected = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, remaining(deadline).map_err(Error::Unreachable)?)
        {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(err) => last_error = err,
        }
    }
    let tcp = connected.ok_or(Error::Unreachable(last_error))?;
    tcp.set_nodelay(true).map_err(Error::Unreachable)?;
    // Bound both protocol buffers before accepting peer WebSocket traffic.
    let config = WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (socket, _) = tungstenite::client::client_with_config(
        request,
        Socket::Handshake {
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
    // Transfer the upgraded socket to its worker and give wire blocking adapters.
    let (reader, writer, shutdown) = adapters(socket).map_err(Error::Unreachable)?;
    Ark::attach(transport::Stream::new(reader, writer, shutdown), verifier)
}

/// Socket used during the blocking HTTP upgrade and subsequent nonblocking I/O.
/// Replacing its adapter retains bytes already buffered by the WebSocket.
enum Socket {
    /// TCP stream whose individual I/O calls share the upgrade deadline.
    Handshake {
        stream: TcpStream,
        deadline: Instant,
    },
    /// Registered socket whose I/O rearms readiness through mio.
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Handshake { stream, deadline } => {
                stream.set_read_timeout(Some(remaining(*deadline)?))?;
                stream.read(buf)
            }
            Self::Connected(stream) => stream.read(buf),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Handshake { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.write(buf)
            }
            Self::Connected(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Handshake { stream, .. } => stream.flush(),
            Self::Connected(stream) => stream.flush(),
        }
    }
}

/// Returns the time left before a deadline, refusing an already expired budget.
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
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

/// Whether the WebSocket needs a readiness event before it can make progress.
fn would_block(err: &tungstenite::Error) -> bool {
    matches!(err, tungstenite::Error::Io(err) if err.kind() == io::ErrorKind::WouldBlock)
}

/// Binary input waiting for the reader, followed by the worker's ending result.
#[derive(Default)]
struct Incoming {
    chunks: VecDeque<Bytes>,  // Messages retained until the reader consumes them
    bytes: usize,             // Payload bytes still charged against the input limit
    ended: bool,              // Whether the worker has stopped producing input
    error: Option<io::Error>, // Ending failure, returned after buffered input
}

/// Input and closure state shared by the worker and its blocking adapters.
struct Shared {
    closed: AtomicBool,        // Local shutdown signal checked by every participant
    incoming: Mutex<Incoming>, // Buffered input and the worker's ending result
    available: Condvar,        // Wakes the reader for input, failure or closure
    wake: Waker,               // Wakes the worker when an adapter changes its work
}

impl Shared {
    /// Marks local closure and wakes both the reader and the socket worker.
    fn close(&self) {
        // Pair the condition change with the reader's lock to prevent a lost wake.
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

    /// Reserves room for a complete message before the worker reads another one.
    /// The chunk limit also bounds overhead from many small binary messages.
    fn can_read(&self) -> bool {
        let incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        incoming.bytes <= INBOUND_LIMIT - MAX_MESSAGE && incoming.chunks.len() < 1024
    }

    /// Queues a binary message and wakes the reader. Empty messages carry no bytes.
    fn push(&self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        let mut incoming = self.incoming.lock().expect("WebSocket input not poisoned");
        incoming.bytes += bytes.len();
        incoming.chunks.push_back(bytes);
        self.available.notify_one();
    }
}

/// One flush submitted by the writer, acknowledged when the worker finishes it.
struct Outgoing {
    bytes: Vec<u8>,    // Complete frame accumulated since the previous flush
    deadline: Instant, // Original write deadline, including the queue wait
    /// Receives the local write result without blocking the socket worker.
    done: mpsc::SyncSender<io::Result<()>>,
}

/// Starts the socket worker and returns adapters with their shutdown operation.
/// The returned shutdown wakes blocking I/O and closes the underlying socket.
fn adapters(mut socket: WebSocket<Socket>) -> io::Result<(Reader, Writer, impl FnOnce() + Send)> {
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
    // Reads and writes must use mio's socket, which rearms readiness on Windows.
    *socket.get_mut() = Socket::Connected(connected);
    let shared = Arc::new(Shared {
        closed: AtomicBool::new(false),
        incoming: Mutex::new(Incoming::default()),
        available: Condvar::new(),
        wake: Waker::new(poll.registry(), WAKE)?,
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

/// Drives the WebSocket while preserving input progress during blocked output.
/// One adapter writer submits flushes serially and waits for each acknowledgement.
fn pump(
    mut socket: WebSocket<Socket>,
    mut poll: Poll,
    shared: Arc<Shared>,
    outbox: mpsc::Receiver<Outgoing>,
) {
    let mut events = Events::with_capacity(8);
    let mut active: Option<Outgoing> = None;
    let mut control_deadline = None;
    let mut peer_closed = false;
    let result = (|| -> io::Result<()> {
        loop {
            if shared.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(frame) = &active {
                remaining(frame.deadline)?;
            }
            if let Some(deadline) = control_deadline {
                remaining(deadline)?;
            }
            // Admit at most one flush. Its bytes stay in the WebSocket until
            // output completes, while the original deadline continues to run.
            if active.is_none() && !peer_closed {
                match outbox.try_recv() {
                    Ok(mut frame) => {
                        if remaining(frame.deadline).is_err() {
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
            // Limit each batch so continuous ingress cannot starve sends or deadlines.
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
                    control_deadline = None;
                    if let Some(frame) = active.take() {
                        let _ = frame.done.send(Ok(()));
                    }
                    if peer_closed {
                        return Ok(());
                    }
                }
                Err(err) if would_block(&err) => {
                    control_deadline
                        .get_or_insert_with(|| Instant::now() + transport::DEFAULT_WRITE_TIMEOUT);
                }
                Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
                Err(err) => return Err(socket_error(err)),
            }
            if batch_full {
                continue;
            }
            // Either socket readiness or an adapter wake may enable more work.
            // Deadlines must also wake an otherwise idle or blocked connection.
            let deadline = active
                .as_ref()
                .map(|frame| frame.deadline)
                .into_iter()
                .chain(control_deadline)
                .min();
            let timeout =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            match poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    })();
    // Settle the outstanding flush before waking the reader with the ending result.
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
    shared: Arc<Shared>,       // Input queue and closure notifications
    deadline: Option<Instant>, // Deadline bounding the next wait for input
}

impl Read for Reader {
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
                    self.shared
                        .available
                        .wait_timeout(incoming, remaining(deadline)?)
                        .expect("WebSocket input not poisoned")
                        .0
                }
            };
        }
    }
}

impl transport::Read for Reader {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.deadline = deadline;
        Ok(())
    }
}

/// Blocking writer whose flush waits for the socket worker's completion result.
struct Writer {
    shared: Arc<Shared>,              // Closure signal and worker wakeup
    outgoing: mpsc::Sender<Outgoing>, // Frames waiting for the socket worker
    pending: Vec<u8>,                 // Bytes accumulated since the last flush
    deadline: Option<Instant>,        // One budget for writes and their flush
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if let Some(deadline) = self.deadline {
            remaining(deadline)?;
        }
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + transport::DEFAULT_WRITE_TIMEOUT);
        remaining(deadline)?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let (done, result) = mpsc::sync_channel(1);
        self.outgoing
            .send(Outgoing {
                bytes: std::mem::take(&mut self.pending),
                deadline,
                done,
            })
            .map_err(|_| closed())?;
        self.shared.wake.wake()?;
        match result.recv_timeout(remaining(deadline)?) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::from(io::ErrorKind::TimedOut)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }
}

impl transport::Write for Writer {
    fn set_write_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        self.deadline = Some(deadline);
        Ok(())
    }
}

/// WebSocket control, buffering, deadline and wire session regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Peer, answering, hangup};
    use darkbio_wire::memory::Duplex;
    use std::net::TcpListener;
    use std::thread;

    /// Exercises the actual adapters without a wire session consuming their bytes.
    fn socket_pair() -> (WebSocket<Socket>, WebSocket<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            tcp.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            tungstenite::accept(tcp).unwrap()
        });
        let tcp = TcpStream::connect(addr).unwrap();
        let (client, _) = tungstenite::client(
            format!("ws://{addr}/v1/usb"),
            Socket::Handshake {
                stream: tcp,
                deadline: Instant::now() + Duration::from_secs(2),
            },
        )
        .unwrap();
        (client, server.join().unwrap())
    }

    /// Ping and Close receive protocol replies while the application is idle.
    #[test]
    fn test_control_frames() {
        let (socket, mut server) = socket_pair();
        let (mut reader, _writer, shutdown) = adapters(socket).unwrap();
        transport::Read::set_read_deadline(
            &mut reader,
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap();
        server
            .send(Message::Ping(Bytes::from_static(b"probe")))
            .unwrap();
        assert_eq!(
            server.read().unwrap(),
            Message::Pong(Bytes::from_static(b"probe"))
        );
        server.close(None).unwrap();
        assert!(matches!(server.read().unwrap(), Message::Close(_)));
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        shutdown();
    }

    /// A read respects its deadline, and shutdown also wakes a read without one.
    #[test]
    fn test_read_deadline_and_close() {
        let (socket, _server) = socket_pair();
        let (mut reader, _writer, shutdown) = adapters(socket).unwrap();
        transport::Read::set_read_deadline(
            &mut reader,
            Some(Instant::now() + Duration::from_millis(20)),
        )
        .unwrap();
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        transport::Read::set_read_deadline(&mut reader, None).unwrap();
        let reading = thread::spawn(move || reader.read(&mut [0]));
        shutdown();
        assert_eq!(reading.join().unwrap().unwrap(), 0);
    }

    /// Consuming buffered input releases capacity for the remaining messages.
    #[test]
    fn test_input_buffering() {
        let (socket, mut server) = socket_pair();
        let (mut reader, _writer, shutdown) = adapters(socket).unwrap();
        let sending = thread::spawn(move || {
            let message = Bytes::from(vec![7; MAX_MESSAGE]);
            for _ in 0..4 {
                server.send(Message::Binary(message.clone())).unwrap();
            }
            server
        });
        transport::Read::set_read_deadline(
            &mut reader,
            Some(Instant::now() + Duration::from_secs(3)),
        )
        .unwrap();
        let mut message = vec![0; MAX_MESSAGE];
        for _ in 0..4 {
            reader.read_exact(&mut message).unwrap();
            assert!(message.iter().all(|byte| *byte == 7));
            assert!(reader.shared.incoming.lock().unwrap().bytes <= INBOUND_LIMIT);
        }
        let _server = sending.join().unwrap();
        shutdown();
    }

    /// Blocked output retains its deadline while incoming traffic still reaches the reader.
    #[test]
    fn test_output_backpressure() {
        let (socket, mut server) = socket_pair();
        let (_reader, mut writer, shutdown) = adapters(socket).unwrap();
        let shared = writer.shared.clone();
        let (started, writing) = mpsc::channel();
        let sending = thread::spawn(move || -> io::Result<()> {
            let deadline = Instant::now() + Duration::from_millis(300);
            transport::Write::set_write_deadline(&mut writer, deadline).unwrap();
            started.send(()).unwrap();
            let message = vec![9; MAX_MESSAGE];
            loop {
                writer.write_all(&message)?;
                writer.flush()?;
            }
        });
        writing.recv().unwrap();
        // The peer sends input but deliberately never drains the client's output.
        server
            .send(Message::Binary(Bytes::from_static(b"incoming")))
            .unwrap();
        let mut incoming = shared.incoming.lock().unwrap();
        while incoming.bytes == 0 && !incoming.ended {
            incoming = shared
                .available
                .wait_timeout(incoming, Duration::from_secs(1))
                .unwrap()
                .0;
        }
        assert_eq!(incoming.chunks.front().unwrap().as_ref(), b"incoming");
        drop(incoming);
        assert_eq!(
            sending.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        shutdown();
    }

    /// How often the bridge turns from one direction to the other.
    const ROUND: Duration = Duration::from_millis(10);

    // Serves one WebSocket client on the listener the way an emulator does,
    // carrying its binary messages into the peer's stream and the stream's
    // bytes back out as messages, until either side ends.
    fn bridge(listener: TcpListener, stream: Duplex) {
        let (mut reader, mut writer) = stream.into_halves();
        let (tcp, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();
        socket
            .send(Message::Ping(Bytes::from_static(b"keepalive")))
            .unwrap();
        let mut pong_received = false;
        socket.get_ref().set_read_timeout(Some(ROUND)).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
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
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
            transport::Read::set_read_deadline(&mut reader, Some(Instant::now() + ROUND)).unwrap();
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if socket
                        .send(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
        assert!(
            pong_received,
            "client must answer Ping while carrying wire traffic"
        );
    }

    // Tests that a session over an emulator's socket reaches the peer behind
    // it, the requests answered and the close ending the socket.
    #[test]
    fn test_socket_session() {
        let mut peer = Peer::spawn(Box::new(answering));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/usb", listener.local_addr().unwrap());
        let stream = peer.stream();
        let served = thread::spawn(move || bridge(listener, stream));

        let (ark, _) = connect(
            &url,
            &crate::TrustMode::Recover(Box::new(peer.identity.clone())),
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

    // Tests that the emulator going away ends the session with the reason,
    // the reading object reporting the end of the stream once the socket
    // behind it is gone.
    #[test]
    fn test_socket_lost() {
        let mut peer = Peer::spawn(hangup());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/usb", listener.local_addr().unwrap());
        let stream = peer.stream();
        let served = thread::spawn(move || bridge(listener, stream));

        let (ark, _) = connect(
            &url,
            &crate::TrustMode::Recover(Box::new(peer.identity.clone())),
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
