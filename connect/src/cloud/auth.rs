// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Caller-supplied authentication for protected cloud hosts.

use super::Failure;
use crate::Timing;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use ureq::http::{HeaderMap, StatusCode};

/// Caller-owned authentication for a cloud host. Connect supplies the HTTPS
/// origin; credential storage, response recognition and login stay with the caller.
/// The same headers are used for HTTP requests and WebSocket upgrades.
pub trait CloudAuth: Send + Sync {
    /// Returns cached authentication headers without prompting, or an empty map.
    /// The deadline bounds lookup. Headers must not replace protocol headers.
    fn headers(&self, origin: &str, deadline: Instant) -> HeaderMap;

    /// Whether a response refused caller authentication before reaching the cloud.
    /// A device proof rejection must return false so cloud key refresh stays separate.
    fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool;

    /// Obtains fresh authentication headers. The optional absolute deadline must
    /// be honored; the caller chooses its login window and interaction policy.
    /// Failure diagnostics must never contain credentials.
    fn login(&self, origin: &str, deadline: Option<Instant>) -> Result<HeaderMap, String>;
}

/// Shared provider and its cached headers, installed before cloud operations.
#[derive(Default)]
pub(super) struct Authorization {
    provider: RwLock<Option<Arc<dyn CloudAuth>>>, // Caller policy, copied before callbacks
    cached: RwLock<Option<HeaderMap>>,            // Headers reused until the host refuses them
}

impl std::fmt::Debug for Authorization {
    /// Omits the caller's provider and credentials from session diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorization").finish_non_exhaustive()
    }
}

impl Authorization {
    /// Whether the caller supplied authentication for this cloud host.
    pub(super) fn configured(&self) -> bool {
        self.provider().is_some()
    }

    /// Replaces the provider without invoking it or starting network I/O.
    pub(super) fn set(&self, provider: Arc<dyn CloudAuth>) {
        *self
            .provider
            .write()
            .expect("cloud credentials not poisoned") = Some(provider);
        *self.cached.write().expect("cloud credentials not poisoned") = None;
    }

    /// Copies the provider so callbacks never hold the configuration lock.
    fn provider(&self) -> Option<Arc<dyn CloudAuth>> {
        self.provider
            .read()
            .expect("cloud credentials not poisoned")
            .clone()
    }

    /// Reads credentials once, outside the lock. A concurrent login takes priority.
    pub(super) fn headers(&self, origin: &str, deadline: Instant) -> HeaderMap {
        if let Some(cached) = &*self.cached.read().expect("cloud credentials not poisoned") {
            return cached.clone();
        }
        let headers = self.provider().map_or_else(HeaderMap::new, |provider| {
            sensitive(provider.headers(origin, deadline))
        });
        let mut cached = self.cached.write().expect("cloud credentials not poisoned");
        cached.get_or_insert(headers).clone()
    }

    /// Defers gateway response recognition to the caller's authentication policy.
    pub(super) fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool {
        self.provider()
            .is_some_and(|provider| provider.rejected(origin, status, headers))
    }

    /// Refreshes credentials without extending an absolute operation deadline.
    pub(super) fn login(&self, origin: &str, timing: Timing) -> Result<(), Failure> {
        let check = || {
            timing
                .check()
                .map_err(|_| Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        };
        check()?;
        let provider = self.provider().ok_or_else(|| Failure::CloudAuth {
            origin: origin.into(),
            message: "cloud access requires login".into(),
        })?;
        let headers = provider.login(origin, timing.deadline());
        check()?;
        let headers = headers.map_err(|message| Failure::CloudAuth {
            origin: origin.into(),
            message,
        })?;
        *self.cached.write().expect("cloud credentials not poisoned") = Some(sensitive(headers));
        Ok(())
    }
}

/// Redacts every caller-supplied authentication value from HTTP debug output.
fn sensitive(mut headers: HeaderMap) -> HeaderMap {
    for value in headers.values_mut() {
        value.set_sensitive(true);
    }
    headers
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Local authentication policy used by HTTP, socket and firmware tests.
    #[derive(Clone, Default)]
    pub(in crate::cloud) struct Login {
        pub lookups: Arc<AtomicUsize>,
        pub logins: Arc<AtomicUsize>,
        pub delay: Duration,
        pub fail: bool,
    }

    impl CloudAuth for Login {
        fn headers(&self, _: &str, _: Instant) -> HeaderMap {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            HeaderMap::from_iter([("authorization".parse().unwrap(), "cached".parse().unwrap())])
        }

        fn rejected(&self, _: &str, _: StatusCode, headers: &HeaderMap) -> bool {
            headers.contains_key("x-test-auth")
        }

        fn login(&self, _: &str, deadline: Option<Instant>) -> Result<HeaderMap, String> {
            self.logins.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(deadline.map_or(self.delay, |deadline| {
                self.delay
                    .min(deadline.saturating_duration_since(Instant::now()))
            }));
            if self.fail {
                return Err("test login refused".into());
            }
            Ok(HeaderMap::from_iter([(
                "authorization".parse().unwrap(),
                "refreshed".parse().unwrap(),
            )]))
        }
    }

    pub(in crate::cloud) fn refused(status: u16) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nX-Test-Auth: required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    #[test]
    fn credentials_are_cached_redacted_and_refreshed() {
        let auth = Authorization::default();
        let login = Login::default();
        auth.set(Arc::new(login.clone()));
        assert_eq!(login.lookups.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        for _ in 0..2 {
            let headers = auth.headers("https://test.invalid", deadline);
            assert_eq!(headers["authorization"], "cached");
            assert!(headers["authorization"].is_sensitive());
            assert!(!format!("{headers:?}").contains("cached"));
        }
        auth.login("https://test.invalid", deadline.into()).unwrap();
        let headers = auth.headers("https://test.invalid", deadline);
        assert_eq!(headers["authorization"], "refreshed");
        assert!(headers["authorization"].is_sensitive());
        assert!(!format!("{auth:?}").contains("refreshed"));
        assert_eq!(login.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(login.logins.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn login_retains_absolute_deadlines() {
        let auth = Authorization::default();
        let login = Login {
            delay: Duration::from_millis(100),
            ..Default::default()
        };
        auth.set(Arc::new(login.clone()));
        assert!(matches!(
            auth.login("https://test.invalid", Timing::until(Instant::now())),
            Err(Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        ));
        assert_eq!(login.logins.load(Ordering::SeqCst), 0);
        let timing = Timing::inactivity(Duration::from_millis(1));
        auth.login("https://test.invalid", timing).unwrap();
        assert!(matches!(
            auth.login(
                "https://test.invalid",
                timing.with_deadline(Instant::now() + Duration::from_millis(5))
            ),
            Err(Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        ));
    }
}
