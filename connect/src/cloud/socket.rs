// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Authenticated cloud sockets with blocking deadlines and relay readiness.

use super::{Failure, dns};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use darkbio_wire::protocol;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::{
    WebSocket, client::IntoClientRequest, handshake::HandshakeError, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;

pub(super) type Connection = WebSocket<MaybeTlsStream<Socket>>;

/// Opens a cloud socket and requires the requested application subprotocol.
pub(super) fn connect(
    url: &str,
    auth: &[u8],
    subprotocol: &str,
    deadline: Instant,
) -> Result<Connection, Failure> {
    let mut request = url.into_client_request().map_err(socket_error)?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!(
            "{subprotocol}, Dark-Auth|{}",
            BASE64_URL_SAFE_NO_PAD.encode(auth)
        )
        .parse()
        .expect("base64url is a valid header"),
    );
    let host = request
        .uri()
        .host()
        .ok_or_else(|| Failure::Cloud("cloud socket URL has no host".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = request
        .uri()
        .port_u16()
        .unwrap_or(if request.uri().scheme_str() == Some("wss") {
            443
        } else {
            80
        });

    let addresses = dns::resolve(&host, port, deadline)?;
    let mut failure = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "cloud socket has no address",
    );
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
    let (socket, response) = tungstenite::client_tls_with_config(
        request,
        Socket::Blocking { stream, deadline },
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
        != Some(subprotocol)
    {
        return Err(Failure::Cloud(format!(
            "cloud did not select the {subprotocol} subprotocol"
        )));
    }
    Ok(socket)
}

/// Blocking reads and writes share a deadline; attached relays use readiness.
#[derive(Debug)]
pub(super) enum Socket {
    Blocking {
        stream: TcpStream,
        deadline: Instant,
    },
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Blocking { stream, deadline } => {
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
            Self::Blocking { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.write(bytes)
            }
            Self::Connected(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Blocking { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.flush()
            }
            Self::Connected(stream) => stream.flush(),
        }
    }
}

/// Accesses the readiness adapter under either the cleartext test socket or TLS.
pub(super) fn socket_mut(socket: &mut WebSocket<MaybeTlsStream<Socket>>) -> &mut Socket {
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
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            Failure::Wire(protocol::Error::Timeout)
        }
        _ => Failure::Cloud(error.to_string()),
    }
}

pub(super) fn socket_error(error: tungstenite::Error) -> Failure {
    match error {
        tungstenite::Error::Io(error) => io_error(error),
        tungstenite::Error::Http(response) if response.status() == 403 => Failure::ProofRejected,
        error => Failure::Cloud(error.to_string()),
    }
}
