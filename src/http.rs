// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Public downloads with a timeout on each expected network response.

use crate::error::Error;
use std::time::Duration;
use ureq::unversioned::{
    resolver::DefaultResolver,
    transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport},
};

/// Creates an HTTPS download client with bounded network waits and caller-selected
/// redirect allowance. Active bodies can outlive many inactivity windows.
pub(crate) fn agent(timeout: Duration, redirects: u32) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(redirects)
        .http_status_as_error(false)
        .timeout_resolve(Some(timeout))
        .timeout_connect(Some(timeout))
        .timeout_send_request(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default().chain(Inactivity(timeout)),
        DefaultResolver::default(),
    )
}

/// Maps request failures to timeout or cloud reachability actions.
pub(crate) fn error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Timeout(_) => Error::new(7, "timeout", "a network response timed out"),
        error => Error::new(4, "cloud-unreachable", error.to_string()),
    }
}

/// Body readers wrap ureq's typed timeout in io::ErrorKind::Other.
/// Normalize it before handing the reader to a protocol-only workflow.
pub(crate) fn normalize_read_error(error: std::io::Error) -> std::io::Error {
    if matches!(
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>()),
        Some(ureq::Error::Timeout(_))
    ) {
        std::io::Error::new(std::io::ErrorKind::TimedOut, error)
    } else {
        error
    }
}

/// Classifies an HTTP body read after recovering any wrapped timeout.
pub(crate) fn read_error(error: std::io::Error) -> Error {
    let error = normalize_read_error(error);
    if error.kind() == std::io::ErrorKind::TimedOut {
        Error::new(7, "timeout", "a network response timed out")
    } else {
        Error::new(4, "cloud-unreachable", error.to_string())
    }
}

/// Ureq's body timeout covers the entire body. This adapter instead limits each
/// transport wait, preserving any shorter deadline supplied by the HTTP layer.
#[derive(Debug)]
struct Inactivity(Duration);
impl<T: Transport> Connector<T> for Inactivity {
    /// Original transport with a renewed bound on each wait.
    type Out = Idle<T>;
    /// Wraps an established transport without initiating another connection.
    fn connect(
        &self,
        _: &ConnectionDetails,
        transport: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(transport.map(|inner| Idle {
            inner,
            timeout: self.0,
        }))
    }
}
/// HTTP transport that clips each I/O wait to the download inactivity allowance.
#[derive(Debug)]
struct Idle<T> {
    /// Connected transport retaining the HTTP library's buffers and TLS state.
    inner: T,
    /// Fresh allowance for each transport wait, independent of body length.
    timeout: Duration,
}
impl<T: Transport> Idle<T> {
    /// Preserves an earlier HTTP deadline, otherwise applying the inactivity limit.
    fn bound(&self, timeout: NextTimeout) -> NextTimeout {
        if timeout.after > self.timeout.into() {
            NextTimeout {
                after: self.timeout.into(),
                reason: ureq::Timeout::RecvBody,
            }
        } else {
            timeout
        }
    }
}
impl<T: Transport> Transport for Idle<T> {
    /// Uses the original transport buffers without introducing another body copy.
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    /// Writes buffered request bytes under the earlier transport bound.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, self.bound(timeout))
    }
    /// Waits for more response bytes with a renewed inactivity allowance.
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.inner.await_input(self.bound(timeout))
    }
    /// Defers pooled-connection liveness checks to the underlying transport.
    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }
    /// Preserves the transport's TLS status for HTTPS policy checks.
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };

    /// Active downloads can take many inactivity windows. A silent body still
    /// expires, including ureq's wrapped io::Error timeout representation.
    #[test]
    fn body_timeout_measures_each_wait() {
        for stalled in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/reference", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n")
                    .unwrap();
                for _ in 0..8 {
                    thread::sleep(if stalled {
                        Duration::from_millis(600)
                    } else {
                        Duration::from_millis(75)
                    });
                    if stream.write_all(&[42]).is_err() {
                        break;
                    }
                }
            });
            let agent = agent(Duration::from_millis(300), 5);
            let mut response = agent
                .get(&url)
                .config()
                .https_only(false)
                .proxy(None)
                .build()
                .call()
                .unwrap();
            let start = Instant::now();
            let mut bytes = Vec::new();
            let result = response.body_mut().as_reader().read_to_end(&mut bytes);
            if stalled {
                assert_eq!(read_error(result.unwrap_err()).class, 7);
            } else {
                result.unwrap();
                assert_eq!(bytes, [42; 8]);
                assert!(start.elapsed() > Duration::from_millis(300));
            }
            drop(response);
            server.join().unwrap();
        }
    }

    #[test]
    fn public_downloads_require_https() {
        assert!(
            agent(Duration::from_secs(1), 5)
                .get("http://127.0.0.1:1/reference")
                .call()
                .is_err()
        );
    }
}
