// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Shares unfinished DNS lookups across cloud socket attempts.

use super::{Failure, socket};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::Instant;

/// Process-wide sharing of system lookups that cannot be cancelled on timeout.
static RESOLVER: LazyLock<Arc<Resolver>> = LazyLock::new(|| Arc::new(Resolver::default()));

/// Coalesces unfinished lookups by host and port, without caching completed DNS.
#[derive(Default)]
struct Resolver {
    pending: Mutex<HashMap<(String, u16), Arc<Lookup>>>, // Only unfinished system calls
}

/// One system lookup retained until every attached waiter releases it.
#[derive(Default)]
struct Lookup {
    /// Addresses or failure, published once by the resolver worker.
    result: Mutex<Option<Result<Vec<SocketAddr>, Failure>>>,
    /// Wakes all callers when the shared system lookup returns.
    ready: Condvar,
}

/// A caller's deadline ends its wait, leaving the lookup available to retries.
pub(super) fn resolve(
    host: &str,
    port: u16,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, Failure> {
    socket::remaining(deadline).map_err(socket::io_error)?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let name = host.to_owned();
    RESOLVER.resolve((name.clone(), port), deadline, move || {
        (name.as_str(), port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect())
            .map_err(socket::io_error)
    })
}

impl Resolver {
    /// Completed lookups are removed so a later attachment refreshes DNS. An
    /// expired waiter neither cancels nor replaces a system call still running.
    fn resolve(
        self: &Arc<Self>,
        key: (String, u16),
        deadline: Instant,
        lookup: impl FnOnce() -> Result<Vec<SocketAddr>, Failure> + Send + 'static,
    ) -> Result<Vec<SocketAddr>, Failure> {
        socket::remaining(deadline).map_err(socket::io_error)?;
        let mut pending = self.pending.lock().expect("DNS lookups not poisoned");
        let attempt = match pending.get(&key) {
            Some(attempt) => attempt.clone(),
            None => {
                let attempt = Arc::new(Lookup::default());
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
            let left = socket::remaining(deadline).map_err(socket::io_error)?;
            result = attempt
                .ready
                .wait_timeout(result, left)
                .expect("DNS result not poisoned")
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_wire::protocol;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Retries join a stalled lookup, and the next completed attempt refreshes it.
    #[test]
    fn test_timeout_and_retry() {
        let resolver = Arc::new(Resolver::default());
        let key = ("relay.invalid".into(), 443);
        let (release, pause) = mpsc::channel();
        let address: SocketAddr = "127.0.0.1:443".parse().unwrap();
        assert!(matches!(
            resolver.resolve(
                key.clone(),
                Instant::now() + Duration::from_millis(20),
                move || {
                    pause.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(vec![address])
                }
            ),
            Err(Failure::Wire(protocol::Error::Timeout))
        ));
        for _ in 0..3 {
            assert!(matches!(
                resolver.resolve(
                    key.clone(),
                    Instant::now() + Duration::from_millis(20),
                    || panic!("duplicate DNS lookup")
                ),
                Err(Failure::Wire(protocol::Error::Timeout))
            ));
        }
        let attempt = resolver.pending.lock().unwrap().get(&key).unwrap().clone();
        release.send(()).unwrap();
        let result = attempt.result.lock().unwrap();
        let (result, timeout) = attempt
            .ready
            .wait_timeout_while(result, Duration::from_secs(5), |result| result.is_none())
            .unwrap();
        assert!(!timeout.timed_out());
        assert_eq!(result.as_ref().unwrap().as_ref().unwrap(), &[address]);
        drop(result);
        assert!(resolver.pending.lock().unwrap().is_empty());
        assert!(
            resolver
                .resolve(key, Instant::now() + Duration::from_secs(5), || Err(
                    Failure::Relay("fresh DNS result".into())
                ))
                .is_err()
        );
    }
}
