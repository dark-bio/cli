// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Emulated Arks over the WebSocket their firmware serves the wire's byte
//! stream on, what a USB bulk endpoint carries on real hardware. Each binary
//! message is a chunk of that stream, one per frame going out, the frame
//! boundaries being the wire's own. The emulator takes one client at a time.
//!
//! The socket is owned the way the firmware owns its end of it: one protocol
//! object reads and another writes, over two handles of the one connection,
//! each blocking on its own direction. The reading object's output is
//! discarded once the upgrade is done, so a reply it might owe, a pong or a
//! close, can never interleave with the writing object's frames.

use crate::ark::{Ark, Realm};
use crate::link::{Link, expired};
use crate::{Device, Error, registry, wire};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::Role;
use tungstenite::{Bytes, HandshakeError, Message, WebSocket};
use wire::transport::{self, Verifier};

/// WebSocket endpoint the Ark emulator exposes by default, on the host address
/// the launcher forwards into the guest.
pub const DEFAULT_URL: &str = "ws://127.0.0.1:18181/v1/usb";

/// Lists the emulators running on the host, as their launchers publish them.
/// None running is an empty list.
pub fn list() -> Result<Vec<Device>, Error> {
    Ok(registry::list()?
        .into_iter()
        .map(Device::emulator)
        .collect())
}

/// Connects to an emulator and runs the wire handshake over the connection,
/// the verifier deciding whether to trust the attestation it presents.
/// Reaching the emulator gets the same budget the wire gives the handshake.
pub fn connect<V: Verifier>(url: &str, verifier: &V) -> Result<(Ark, V::Info), Error> {
    // Resolve the host of the url, plain sockets only as the emulator is local
    let request = url.into_client_request().map_err(Error::Upgrade)?;
    let uri = request.uri();
    if uri.scheme_str() != Some("ws") {
        return Err(Error::Unreachable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "emulator url must be ws://",
        )));
    }
    let host = uri.host().ok_or_else(|| {
        Error::Unreachable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "emulator url without a host",
        ))
    })?;
    let addr = (host, uri.port_u16().unwrap_or(80))
        .to_socket_addrs()
        .map_err(Error::Unreachable)?
        .next()
        .ok_or_else(|| {
            Error::Unreachable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "emulator host resolves to no address",
            ))
        })?;

    // Connect and upgrade within the handshake's budget, the socket's read
    // timeout bounding the upgrade
    let budget = transport::DEFAULT_HANDSHAKE_TIMEOUT;
    let stream = TcpStream::connect_timeout(&addr, budget).map_err(Error::Unreachable)?;
    stream.set_nodelay(true).map_err(Error::Unreachable)?;
    stream
        .set_read_timeout(Some(budget))
        .map_err(Error::Unreachable)?;
    let handle = Handle {
        stream,
        muted: false,
    };
    let (socket, _) = tungstenite::client(request, handle).map_err(|err| match err {
        HandshakeError::Interrupted(_) => Error::Timeout,
        HandshakeError::Failure(err) => Error::Upgrade(err),
    })?;
    attach(socket, verifier)
}

/// Runs the wire handshake over an upgraded socket and wraps the session, the
/// socket split between the reading object the upgrade produced and a writing
/// object over a second handle of the connection. Closing the connection
/// shuts the socket down, which releases whichever direction is blocked.
fn attach<V: Verifier>(
    mut socket: WebSocket<Handle>,
    verifier: &V,
) -> Result<(Ark, V::Info), Error> {
    let sender = socket
        .get_ref()
        .stream
        .try_clone()
        .map_err(Error::Unreachable)?;
    let closing = socket
        .get_ref()
        .stream
        .try_clone()
        .map_err(Error::Unreachable)?;
    socket.get_mut().muted = true;

    let link = Arc::new(Link::new());
    let reader: Box<dyn transport::Read + Send> = Box::new(Reader {
        socket,
        link: link.clone(),
        served: Bytes::new(),
        offset: 0,
        deadline: None,
    });
    let writer: Box<dyn transport::Write + Send> = Box::new(Writer {
        socket: WebSocket::from_raw_socket(sender, Role::Client, None),
        link: link.clone(),
        pending: Vec::new(),
        deadline: None,
    });

    // Shutdown ends the read and the send blocked on the socket, a read then
    // reporting the end of the stream and a write refusing. The wire's closer
    // waits for these calls before reporting the stream closed.
    let stream = transport::Stream::new(reader, writer, move || {
        link.close();
        let _ = closing.shutdown(Shutdown::Both);
    });
    Ark::attach(stream, Realm::Sandbox, verifier)
}

/// Handle of the connection the reading object owns, its output discarded
/// once the upgrade is done, so the object's own frames never interleave with
/// the writing object's.
struct Handle {
    stream: TcpStream, // The connection
    muted: bool,       // Whether output is discarded
}

impl Read for Handle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.muted {
            return Ok(buf.len());
        }
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.muted {
            return Ok(());
        }
        self.stream.flush()
    }
}

/// Time left until the deadline as a socket timeout, a passed one being a
/// timeout already.
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

/// The error of output refused once the connection was closed.
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "socket closed")
}

/// Reader over the reading object, each binary message served as a run of
/// the byte stream. The socket's read timeout bounds a wait by the deadline
/// the wire installed, none meaning the wait lasts until a message arrives
/// or the connection ends. The emulator going away ends the stream, as does
/// the close.
struct Reader {
    socket: WebSocket<Handle>, // Reading object, its output discarded
    link: Arc<Link>,           // Close signal
    served: Bytes,             // Message being served
    offset: usize,             // Bytes of it served so far
    deadline: Option<Instant>, // Deadline the wire installed for its reads
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let left = &self.served[self.offset..];
            if !left.is_empty() {
                let n = buf.len().min(left.len());
                buf[..n].copy_from_slice(&left[..n]);
                self.offset += n;
                return Ok(n);
            }
            if self.link.closed() {
                return Ok(0);
            }
            let timeout = self.deadline.map(remaining).transpose()?;
            self.socket.get_ref().stream.set_read_timeout(timeout)?;
            match self.socket.read() {
                Ok(Message::Binary(data)) => {
                    self.served = data;
                    self.offset = 0;
                }
                Ok(Message::Close(_)) => return Ok(0),
                Ok(_) => {}
                Err(_) if self.link.closed() => return Ok(0),
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(0);
                }
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                Err(tungstenite::Error::Io(err)) => return Err(err),
                Err(err) => return Err(io::Error::other(err)),
            }
        }
    }
}

impl transport::Read for Reader {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.deadline = deadline;
        Ok(())
    }
}

/// Writer over the writing object, the writes gathered and a flush sending
/// them as one message, one frame. The socket's write timeout bounds the send
/// by the deadline the wire installed, a send running out leaving its rest
/// queued in the object ahead of the next. The close refuses further output.
struct Writer {
    socket: WebSocket<TcpStream>, // Writing object over its own handle
    link: Arc<Link>,              // Close signal
    pending: Vec<u8>,             // Bytes written since the last flush
    deadline: Option<Instant>,    // Deadline the wire installed for its writes
}

impl Writer {
    /// Maps a failed send to the error the wire reports, the socket's timeout
    /// running out being the deadline passing.
    fn failed(err: tungstenite::Error) -> io::Error {
        match err {
            tungstenite::Error::Io(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                io::Error::from(io::ErrorKind::TimedOut)
            }
            tungstenite::Error::Io(err) => err,
            err => io::Error::other(err),
        }
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.link.closed() {
            return Err(closed());
        }
        if expired(self.deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.link.closed() {
            return Err(closed());
        }
        let timeout = self.deadline.map(remaining).transpose()?;
        self.socket.get_ref().set_write_timeout(timeout)?;
        if self.pending.is_empty() {
            // Nothing new, only what a send running out left queued
            return self.socket.flush().map_err(Self::failed);
        }
        let message = Message::Binary(Bytes::from(std::mem::take(&mut self.pending)));
        self.socket.send(message).map_err(Self::failed)
    }
}

impl transport::Write for Writer {
    fn set_write_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        self.deadline = Some(deadline);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Peer, answering, hangup};
    use darkbio_wire::memory::Duplex;
    use std::net::TcpListener;
    use std::thread;

    /// How often the bridge turns from one direction to the other.
    const ROUND: Duration = Duration::from_millis(10);

    // Serves one WebSocket client on the listener the way an emulator does,
    // carrying its binary messages into the peer's stream and the stream's
    // bytes back out as messages, until either side ends.
    fn bridge(listener: TcpListener, stream: Duplex) {
        let (mut reader, mut writer) = stream.into_halves();
        let (tcp, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();
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

        let (ark, _) = connect(&url, &peer.identity).unwrap();
        assert_eq!(ark.realm(), Realm::Sandbox);
        assert_eq!(ark.device_info().unwrap().firmware_version, "1.0.0");
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

        let (ark, _) = connect(&url, &peer.identity).unwrap();
        let err = ark.device_info().unwrap_err();
        assert!(matches!(err, Error::Disconnected(_)), "{err:?}");
        served.join().unwrap();
    }
}
