// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Authenticated cloud sockets with blocking deadlines and relay readiness.

use super::{Failure, dns, http::Api};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use darkbio_wire::protocol;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::{
    WebSocket, client::IntoClientRequest, handshake::HandshakeError, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

/// Bounds cloud frames and assembled messages to wire's transport capacity.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;

/// Cloud WebSocket retaining its TLS state when switched to readiness polling.
pub(super) type Connection = WebSocket<MaybeTlsStream<Socket>>;

/// Opens a cloud socket and requires the requested application subprotocol.
/// DNS, TCP, TLS and upgrade share one deadline. Caller authentication stays
/// separate from cloud proof refusals, before any application exchange begins.
pub(super) fn connect(
    api: &Api,
    url: &str,
    auth: &[u8],
    subprotocol: &str,
    deadline: Instant,
) -> Result<Connection, Failure> {
    let mut request = url.into_client_request().map_err(socket_error)?;
    request
        .headers_mut()
        .extend(api.auth.headers(&api.origin, deadline));
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
        HandshakeError::Failure(tungstenite::Error::Http(response))
            if api
                .auth
                .rejected(&api.origin, response.status(), response.headers()) =>
        {
            Failure::AuthRequired
        }
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
    /// Handshake or pairing stream with a shared read and write bound.
    Blocking {
        /// Connected TCP socket, optionally wrapped by TLS above this adapter.
        stream: TcpStream,
        /// Absolute bound checked again before each blocking I/O.
        deadline: Instant,
    },
    /// Attached relay socket serviced by the worker's readiness loop.
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    /// Applies the remaining blocking deadline or returns readiness-based I/O.
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
    /// Applies the remaining blocking deadline or writes through the relay socket.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Blocking { stream, deadline } => {
                stream.set_write_timeout(Some(remaining(*deadline)?))?;
                stream.write(bytes)
            }
            Self::Connected(stream) => stream.write(bytes),
        }
    }
    /// Flushes the underlying stream under the same bound as a write.
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

/// Returns a positive OS timeout; zero would mean an unbounded wait on some APIs.
pub(super) fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

/// Preserves timeout classification across blocking socket error conventions.
pub(super) fn io_error(error: io::Error) -> Failure {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            Failure::Wire(protocol::Error::Timeout)
        }
        _ => Failure::Cloud(error.to_string()),
    }
}

/// Separates expired I/O and rejected proofs from other cloud socket failures.
pub(super) fn socket_error(error: tungstenite::Error) -> Failure {
    match error {
        tungstenite::Error::Io(error) => io_error(error),
        tungstenite::Error::Http(response) if response.status() == 403 => Failure::ProofRejected,
        error => Failure::Cloud(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{
        auth::tests::{Login, refused},
        http::tests::api,
        tests::TIMEOUT,
    };
    use crate::{Timing, trust::Realm};
    use std::net::TcpListener;
    use std::sync::{Arc, atomic::Ordering};
    use std::thread;

    #[test]
    #[allow(clippy::result_large_err)] // Tungstenite's server callback owns its HTTP response.
    fn socket_upgrades_and_reconnects_use_refreshed_credentials() {
        for subprotocol in ["Pairing", "Relaying"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}/v1/{subprotocol}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                for attempt in 0..3 {
                    let (stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                    if attempt == 0 {
                        let mut stream = stream;
                        let mut head = Vec::new();
                        while !head.ends_with(b"\r\n\r\n") {
                            let mut byte = [0];
                            stream.read_exact(&mut byte).unwrap();
                            head.push(byte[0]);
                        }
                        assert!(
                            String::from_utf8(head)
                                .unwrap()
                                .to_ascii_lowercase()
                                .contains("authorization: cached\r\n")
                        );
                        stream.write_all(refused(302).as_bytes()).unwrap();
                        continue;
                    }
                    let socket = tungstenite::accept_hdr(stream,
                        |request: &tungstenite::handshake::server::Request, mut response: tungstenite::handshake::server::Response| {
                            assert_eq!(request.headers()["authorization"], "refreshed");
                            assert_eq!(request.headers()["sec-websocket-protocol"], format!("{subprotocol}, Dark-Auth|-_8"));
                            response.headers_mut().insert("sec-websocket-protocol", subprotocol.parse().unwrap());
                            Ok(response)
                        }).unwrap();
                    drop(socket);
                }
            });
            let cloud = api(url.clone(), Realm::Hardware);
            let login = Login::default();
            cloud.auth.set(Arc::new(login.clone()));
            let timing = Timing::inactivity(TIMEOUT);
            for _ in 0..2 {
                let socket = cloud
                    .with_auth(timing, || {
                        connect(&cloud, &url, &[0xfb, 0xff], subprotocol, timing.io())
                    })
                    .unwrap();
                drop(socket);
            }
            server.join().unwrap();
            assert_eq!(login.logins.load(Ordering::SeqCst), 1);
            assert_eq!(login.lookups.load(Ordering::SeqCst), 1);
        }
    }
}
