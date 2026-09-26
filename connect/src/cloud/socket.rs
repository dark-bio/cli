// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Authenticated cloud sockets with blocking deadlines and relay readiness.

use super::{Failure, http::Api};
use crate::timing::ClockExt;
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use darkbio_clock::Clock;
use darkbio_wire::protocol;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Instant;
use tungstenite::{
    WebSocket, client::IntoClientRequest, handshake::HandshakeError, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

/// Largest cloud frame or assembled message, the most one wire message carries.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;

/// Cloud WebSocket retaining its TLS state when switched to readiness polling.
pub(super) type Connection = WebSocket<MaybeTlsStream<Socket>>;

/// Opens a cloud socket that must agree on the requested subprotocol, carrying
/// the Ark's `auth` proof in the upgrade.
///
/// DNS, TCP, TLS and the upgrade share one deadline. A refusal of the caller's
/// credentials returns [`Failure::AuthRequired`], kept apart from a refused
/// proof, before any application exchange begins.
pub(super) fn connect(
    api: &Api,
    url: &str,
    auth: &[u8],
    subprotocol: &str,
    deadline: Instant,
) -> Result<Connection, Failure> {
    // Carry the caller's credentials and the Ark's proof on the upgrade request
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

    // Take the host without IPv6 brackets, and the scheme's default port
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

    // Try the resolved addresses in turn until one connects within the deadline
    let addresses = api.resolver.resolve(&host, port, deadline)?;
    let mut failure = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "cloud socket has no address",
    );
    let mut connected = None;
    for address in addresses {
        let left = api.clock.remaining(deadline).map_err(io_error)?;
        match TcpStream::connect_timeout(&address, left) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => failure = error,
        }
    }
    let stream = connected.ok_or_else(|| io_error(failure))?;
    stream.set_nodelay(true).map_err(io_error)?;

    // Upgrade with frames and messages capped at what one wire message carries
    let config = WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (socket, response) = tungstenite::client_tls_with_config(
        request,
        Socket::Blocking {
            clock: api.clock.clone(),
            stream,
            deadline,
        },
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

    // The cloud must agree on the requested subprotocol
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

/// Stream under a cloud WebSocket, blocking under a deadline or polled for
/// readiness.
///
/// Blocking reads and writes share one deadline, and an attached relay
/// switches to readiness polling.
#[derive(Debug)]
pub(super) enum Socket {
    /// Handshake or pairing stream with a shared read and write bound.
    Blocking {
        /// Clock of the connection, which the deadline is measured on.
        clock: Clock,
        /// Connected TCP socket, optionally wrapped by TLS above this adapter.
        stream: TcpStream,
        /// Absolute bound, turned into a fresh timeout before each blocking I/O.
        deadline: Instant,
    },
    /// Attached relay socket serviced by the worker's readiness loop.
    Connected(mio::net::TcpStream),
}

impl Read for Socket {
    /// Reads within the remaining deadline, or without blocking on a relay
    /// socket.
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Blocking {
                clock,
                stream,
                deadline,
            } => {
                stream.set_read_timeout(Some(clock.remaining(*deadline)?))?;
                stream.read(bytes)
            }
            Self::Connected(stream) => stream.read(bytes),
        }
    }
}

impl Write for Socket {
    /// Writes within the remaining deadline, or without blocking on a relay
    /// socket.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Blocking {
                clock,
                stream,
                deadline,
            } => {
                stream.set_write_timeout(Some(clock.remaining(*deadline)?))?;
                stream.write(bytes)
            }
            Self::Connected(stream) => stream.write(bytes),
        }
    }

    /// Flushes the underlying stream under the same bound as a write.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Blocking {
                clock,
                stream,
                deadline,
            } => {
                stream.set_write_timeout(Some(clock.remaining(*deadline)?))?;
                stream.flush()
            }
            Self::Connected(stream) => stream.flush(),
        }
    }
}

/// Returns the adapter under a WebSocket, reaching through its TLS layer when
/// there is one.
pub(super) fn socket_mut(socket: &mut WebSocket<MaybeTlsStream<Socket>>) -> &mut Socket {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &mut stream.sock,
        _ => unreachable!("only plain and rustls sockets are enabled"),
    }
}

/// Converts an I/O error, treating both timeout kinds as a wire timeout.
///
/// A blocking socket reports an expired timeout as `TimedOut` or `WouldBlock`,
/// depending on the platform.
pub(super) fn io_error(error: io::Error) -> Failure {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            Failure::Wire(protocol::Error::Timeout)
        }
        _ => Failure::Cloud(error.to_string()),
    }
}

/// Converts a WebSocket error, keeping expired I/O and a refused proof apart
/// from other failures.
///
/// An HTTP 403 answer to the upgrade counts as a refused proof.
pub(super) fn socket_error(error: tungstenite::Error) -> Failure {
    match error {
        tungstenite::Error::Io(error) => io_error(error),
        tungstenite::Error::Http(response) if response.status() == 403 => Failure::ProofRejected,
        error => Failure::Cloud(error.to_string()),
    }
}

/// Socket upgrades carrying caller credentials.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{
        auth::tests::{Login, refused},
        http::tests::api,
        tests::TIMEOUT,
    };
    use crate::testing::test_clock;
    use crate::{Timing, trust::Realm};
    use std::net::TcpListener;
    use std::sync::{Arc, atomic::Ordering};
    use std::thread;

    /// A refused upgrade triggers one login, and both the retry and a later
    /// reconnect carry the refreshed credentials.
    #[test]
    #[allow(clippy::result_large_err)] // the upgrade callback's error is a whole HTTP response
    fn socket_upgrades_and_reconnects_use_refreshed_credentials() {
        let clock = test_clock().clock();
        for subprotocol in ["Pairing", "Relaying"] {
            // Refuse the first upgrade for its cached credentials, then accept
            // two that carry refreshed ones
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

            // Connect twice, logging in once when the first upgrade is refused
            let cloud = api(url.clone(), Realm::Hardware, &clock);
            let login = Login::default();
            cloud.auth.set(Arc::new(login.clone()));
            let timing = Timing::inactivity(TIMEOUT);
            for _ in 0..2 {
                let socket = cloud
                    .with_auth(timing, || {
                        connect(&cloud, &url, &[0xfb, 0xff], subprotocol, timing.io(&clock))
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
