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
    agent_over(DefaultConnector::default(), timeout, redirects)
}

/// Builds the download client over the connector that opens its transports,
/// wrapping each one in the inactivity bound.
fn agent_over(connector: impl Connector, timeout: Duration, redirects: u32) -> ureq::Agent {
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
        connector.chain(Inactivity(timeout)),
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
    use crate::testing::wait_deadline;
    use darkbio_clock::{Clock, TestClock, crossbeam_channel};
    use std::io::{self, Read};
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::Instant;
    use ureq::unversioned::transport::LazyBuffers;

    /// Transport whose input the test hands over, waited for on the test clock.
    /// Every wait reports its deadline before it starts.
    #[derive(Debug)]
    struct Scripted {
        /// Clock the input waits run on.
        clock: Clock,
        /// Buffers the client reads and writes through.
        buffers: LazyBuffers,
        /// Input the test hands over.
        input: crossbeam_channel::Receiver<Vec<u8>>,
        /// Deadline of every input wait, reported before it starts.
        waits: mpsc::Sender<Option<Instant>>,
    }

    impl Transport for Scripted {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, _: usize, _: NextTimeout) -> Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            let deadline =
                (!timeout.after.is_not_happening()).then(|| self.clock.now() + *timeout.after);
            self.waits.send(deadline).unwrap();
            let input = match deadline {
                Some(deadline) => self
                    .clock
                    .recv_deadline(&self.input, deadline)
                    .map_err(|_| ureq::Error::Timeout(timeout.reason))?,
                None => self.input.recv().unwrap(),
            };
            self.buffers.input_append_buf()[..input.len()].copy_from_slice(&input);
            self.buffers.input_appended(input.len());
            Ok(true)
        }

        fn is_open(&mut self) -> bool {
            true
        }

        fn is_tls(&self) -> bool {
            true
        }
    }

    /// Opens the scripted transport for the one connection a test makes.
    #[derive(Debug)]
    struct Script(Mutex<Option<Scripted>>);

    impl Connector for Script {
        type Out = Scripted;

        fn connect(
            &self,
            _: &ConnectionDetails,
            _: Option<()>,
        ) -> Result<Option<Scripted>, ureq::Error> {
            Ok(self.0.lock().unwrap().take())
        }
    }

    /// Builds a download client with a 300 ms allowance over a scripted
    /// transport, returning where to hand it input and where its waits report.
    fn scripted(
        clock: &Clock,
    ) -> (
        ureq::Agent,
        crossbeam_channel::Sender<Vec<u8>>,
        mpsc::Receiver<Option<Instant>>,
    ) {
        let (hand, input) = crossbeam_channel::unbounded();
        let (waits, reported) = mpsc::channel();
        let transport = Scripted {
            clock: clock.clone(),
            buffers: LazyBuffers::new(16 * 1024, 16 * 1024),
            input,
            waits,
        };
        let connector = Script(Mutex::new(Some(transport)));
        let agent = agent_over(connector, Duration::from_millis(300), 0);
        (agent, hand, reported)
    }

    /// Active downloads can take many inactivity windows, since each wait for
    /// body input gets the whole allowance. A silent body still expires at the
    /// end of its window, and an earlier HTTP deadline wins over the allowance.
    #[test]
    fn body_timeout_measures_each_wait() {
        // A request's earlier deadline bounds the wait for its response
        let mut tester = TestClock::new();
        let clock = tester.clock();
        let (agent, hand, reported) = scripted(&clock);
        hand.send(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n*".to_vec())
            .unwrap();
        let body = agent
            .get("https://127.0.0.1/reference")
            .config()
            .timeout_global(Some(Duration::from_millis(100)))
            .build()
            .call()
            .unwrap()
            .into_body()
            .read_to_vec()
            .unwrap();
        assert_eq!(body, b"*");
        let response = reported.recv().unwrap().unwrap();
        assert!(response <= clock.now() + Duration::from_millis(100));

        // A body arriving a byte at a time outlives one window, each wait
        // getting the whole allowance
        let (agent, hand, reported) = scripted(&clock);
        hand.send(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n".to_vec())
            .unwrap();
        let reading = thread::spawn(move || {
            let mut response = agent.get("https://127.0.0.1/reference").call().unwrap();
            let mut body = Vec::new();
            let result = response.body_mut().as_reader().read_to_end(&mut body);
            (body, result)
        });
        let response = reported.recv().unwrap().unwrap();
        assert!(response <= clock.now() + Duration::from_millis(300));
        for _ in 0..3 {
            assert_eq!(
                reported.recv().unwrap(),
                Some(clock.now() + Duration::from_millis(300))
            );
            tester.advance(Duration::from_millis(200));
            hand.send(vec![42]).unwrap();
        }

        // The last byte never comes, so its wait expires at the end of its window
        let deadline = reported.recv().unwrap().unwrap();
        assert_eq!(deadline, clock.now() + Duration::from_millis(300));
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        let (body, result) = reading.join().unwrap();
        assert_eq!(body, [42; 3]);
        assert_eq!(read_error(result.unwrap_err()).class, 7);
    }

    /// Body readers wrap ureq's timeout in an io::Error, which still classifies
    /// as a timeout.
    #[test]
    fn test_wrapped_body_timeouts_are_timeouts() {
        let wrapped = ureq::Error::Timeout(ureq::Timeout::RecvBody).into_io();
        assert_eq!(wrapped.kind(), io::ErrorKind::Other);
        assert_eq!(read_error(wrapped).class, 7);
        assert_eq!(
            read_error(io::Error::from(io::ErrorKind::ConnectionReset)).class,
            4
        );
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
