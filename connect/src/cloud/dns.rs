// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Shares unfinished DNS lookups across the cloud socket attempts of a connection.

use super::{Failure, socket};
use crate::timing::ClockExt;
use darkbio_clock::{Clock, sync};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// Coalesces unfinished lookups by host and port, without caching completed DNS.
/// System lookups cannot be cancelled on timeout, so a retry joins the one
/// still running instead of starting another.
#[derive(Debug)]
pub(super) struct Resolver {
    clock: Clock, // clock that the waiters' deadlines are measured on
    pending: Mutex<HashMap<(String, u16), Arc<Lookup>>>, // Only unfinished system calls
}

/// One system lookup retained until every attached waiter releases it.
#[derive(Debug)]
struct Lookup {
    /// Addresses or failure, published once by the resolver worker.
    result: sync::Mutex<Option<Result<Vec<SocketAddr>, Failure>>>,
    /// Wakes all callers when the shared system lookup returns.
    ready: sync::Condvar,
}

impl Resolver {
    /// Creates a resolver whose waiters measure their deadlines on the clock.
    pub(super) fn new(clock: &Clock) -> Arc<Self> {
        Arc::new(Self {
            clock: clock.clone(),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// A caller's deadline ends its wait, leaving the lookup available to retries.
    pub(super) fn resolve(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        deadline: Instant,
    ) -> Result<Vec<SocketAddr>, Failure> {
        self.clock.remaining(deadline).map_err(socket::io_error)?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        let name = host.to_owned();
        self.lookup((name.clone(), port), deadline, move || {
            (name.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect())
                .map_err(socket::io_error)
        })
    }

    /// Completed lookups are removed so a later attachment refreshes DNS. An
    /// expired waiter neither cancels nor replaces a system call still running.
    fn lookup(
        self: &Arc<Self>,
        key: (String, u16),
        deadline: Instant,
        lookup: impl FnOnce() -> Result<Vec<SocketAddr>, Failure> + Send + 'static,
    ) -> Result<Vec<SocketAddr>, Failure> {
        self.clock.remaining(deadline).map_err(socket::io_error)?;
        let mut pending = self.pending.lock().expect("DNS lookups not poisoned");
        let attempt = match pending.get(&key) {
            Some(attempt) => attempt.clone(),
            None => {
                let attempt = Arc::new(Lookup {
                    result: sync::Mutex::new(None),
                    ready: sync::Condvar::new(&self.clock),
                });
                thread::Builder::new()
                    .name("ark-relay-dns".into())
                    .spawn({
                        let resolver = self.clone();
                        let attempt = attempt.clone();
                        let key = key.clone();
                        move || {
                            let result = lookup();
                            let mut pending =
                                resolver.pending.lock().expect("DNS lookups not poisoned");
                            *attempt.result.lock().expect("DNS result not poisoned") = Some(result);
                            pending.remove(&key);
                            attempt.ready.notify_all();
                        }
                    })
                    .map_err(socket::io_error)?;
                pending.insert(key, attempt.clone());
                attempt
            }
        };
        drop(pending);
        let mut result = attempt.result.lock().expect("DNS result not poisoned");
        loop {
            if let Some(result) = &*result {
                return result.clone();
            }
            self.clock.remaining(deadline).map_err(socket::io_error)?;
            result = attempt
                .ready
                .wait_deadline(result, deadline)
                .expect("DNS result not poisoned")
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{test_clock, wait_deadline};
    use darkbio_wire::protocol;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Retries join a stalled lookup, and the next completed attempt refreshes it.
    #[test]
    fn test_timeout_and_retry() {
        // Stall the first system lookup until released
        let mut tester = test_clock();
        let clock = tester.clock();
        let resolver = Resolver::new(&clock);
        let key = ("relay.invalid".to_string(), 443);
        let (started, lookups) = mpsc::channel();
        let (release, pause) = mpsc::channel::<()>();
        let address: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let mut stalled = Some(move || {
            started.send(()).unwrap();
            pause.recv().unwrap();
            Ok(vec![address])
        });

        // The first waiter starts the lookup and three retries join it. Each one
        // parks on its own deadline, which the clock then reaches.
        for attempt in 0..4 {
            let deadline = clock.now() + Duration::from_millis(20);
            let waiter = thread::spawn({
                let resolver = resolver.clone();
                let key = key.clone();
                let stalled = stalled.take();
                move || match stalled {
                    Some(lookup) => resolver.lookup(key, deadline, lookup),
                    None => resolver.lookup(key, deadline, || panic!("duplicate DNS lookup")),
                }
            });
            if attempt == 0 {
                lookups.recv().unwrap();
            }
            wait_deadline(&tester, deadline);
            tester.advance_to(deadline);
            assert!(matches!(
                waiter.join().unwrap(),
                Err(Failure::Wire(protocol::Error::Timeout))
            ));
        }

        // Releasing the lookup publishes its addresses and forgets it
        let attempt = resolver.pending.lock().unwrap().get(&key).unwrap().clone();
        release.send(()).unwrap();
        let result = attempt.result.lock().unwrap();
        let result = attempt
            .ready
            .wait_while(result, |result| result.is_none())
            .unwrap();
        assert_eq!(result.as_ref().unwrap().as_ref().unwrap(), &[address]);
        drop(result);
        assert!(resolver.pending.lock().unwrap().is_empty());

        // A later attempt starts a fresh lookup instead of reusing the finished one
        assert!(
            resolver
                .lookup(key, clock.now() + Duration::from_secs(5), || Err(
                    Failure::Relay("fresh DNS result".into())
                ))
                .is_err()
        );
    }
}
