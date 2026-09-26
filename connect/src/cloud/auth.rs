// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Caller-supplied authentication for protected cloud hosts.

use super::Failure;
use crate::Timing;
use darkbio_clock::Clock;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use ureq::http::{HeaderMap, StatusCode};

/// Caller-owned authentication for a protected cloud host, installed with
/// [`Ark::set_cloud_auth`](crate::Ark::set_cloud_auth).
///
/// Connect supplies the HTTPS origin, while credential storage, response
/// recognition and login stay with the caller. The same headers go on HTTP
/// requests and WebSocket upgrades. Deadlines are measured on the clock of the
/// connection, which [`Client::clock`](crate::Client::clock) returns.
pub trait CloudAuth: Send + Sync {
    /// Returns cached authentication headers without prompting, or an empty map.
    ///
    /// The deadline bounds the lookup. The headers must not replace protocol
    /// headers.
    fn headers(&self, origin: &str, deadline: Instant) -> HeaderMap;

    /// Checks whether a response refused the caller's authentication before
    /// reaching the cloud.
    ///
    /// A refused device proof must return false, so refreshing cloud keys stays
    /// separate from login.
    fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool;

    /// Obtains fresh authentication headers.
    ///
    /// The optional absolute deadline must be honored, while the caller chooses
    /// its own login window and interaction policy. A failure diagnostic must
    /// never contain credentials.
    fn login(&self, origin: &str, deadline: Option<Instant>) -> Result<HeaderMap, String>;
}

/// Shared provider and its cached headers, installed before cloud operations.
#[derive(Default)]
pub(super) struct Authorization {
    /// Caller's provider, copied out before each callback so none runs under
    /// the lock.
    provider: RwLock<Option<Arc<dyn CloudAuth>>>,
    /// Headers reused for every request until a login or a new provider
    /// replaces them.
    cached: RwLock<Option<HeaderMap>>,
}

impl std::fmt::Debug for Authorization {
    /// Omits the caller's provider and credentials from session diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorization").finish_non_exhaustive()
    }
}

impl Authorization {
    /// Checks whether the caller installed an authentication provider.
    pub(super) fn configured(&self) -> bool {
        self.provider().is_some()
    }

    /// Replaces the provider and drops the cached headers, without invoking it
    /// or starting network I/O.
    pub(super) fn set(&self, provider: Arc<dyn CloudAuth>) {
        *self
            .provider
            .write()
            .expect("cloud credentials not poisoned") = Some(provider);
        *self.cached.write().expect("cloud credentials not poisoned") = None;
    }

    /// Copies the provider out, so its callbacks never run under the
    /// configuration lock.
    fn provider(&self) -> Option<Arc<dyn CloudAuth>> {
        self.provider
            .read()
            .expect("cloud credentials not poisoned")
            .clone()
    }

    /// Returns the cached headers, asking the provider once when none are cached.
    ///
    /// The provider runs outside the lock. Headers from a concurrent login take
    /// priority over the ones it returns.
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

    /// Asks the provider whether a response refused the caller's credentials.
    ///
    /// Without a provider, no response counts as a refusal.
    pub(super) fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool {
        self.provider()
            .is_some_and(|provider| provider.rejected(origin, status, headers))
    }

    /// Replaces the cached headers with fresh ones from the provider's login,
    /// within the operation's absolute deadline.
    ///
    /// The deadline is measured on the clock, and an inactivity allowance does
    /// not bound the login. Without a provider, the login fails with
    /// [`Failure::CloudAuth`].
    pub(super) fn login(&self, origin: &str, clock: &Clock, timing: Timing) -> Result<(), Failure> {
        let check = || {
            timing
                .check(clock)
                .map_err(|_| Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        };
        check()?;
        let provider = self.provider().ok_or_else(|| Failure::CloudAuth {
            origin: origin.into(),
            message: "cloud access requires login".into(),
        })?;

        // The provider bounds its own wait, so check the deadline again after it
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

/// Marks every caller-supplied header value sensitive, which redacts it from
/// debug output.
fn sensitive(mut headers: HeaderMap) -> HeaderMap {
    for value in headers.values_mut() {
        value.set_sensitive(true);
    }
    headers
}

/// Credential caching and login deadlines, with the provider stand-in that the
/// other cloud tests share.
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::testing::test_clock;
    use darkbio_clock::TestClock;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Authentication provider stand-in for the cloud tests, counting its
    /// lookups and logins.
    #[derive(Clone, Default)]
    pub(in crate::cloud) struct Login {
        /// Number of cached header lookups.
        pub lookups: Arc<AtomicUsize>,
        /// Number of logins.
        pub logins: Arc<AtomicUsize>,
        /// Whether every login fails.
        pub fail: bool,
        /// Test clock and the time each login spends on it, standing in for the
        /// owner signing in through the browser.
        pub browser: Option<(Arc<Mutex<TestClock>>, Duration)>,
    }

    impl CloudAuth for Login {
        /// Counts the lookup and returns an `authorization: cached` header.
        fn headers(&self, _: &str, _: Instant) -> HeaderMap {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            HeaderMap::from_iter([("authorization".parse().unwrap(), "cached".parse().unwrap())])
        }

        /// Treats any response carrying an `x-test-auth` header as a refusal.
        fn rejected(&self, _: &str, _: StatusCode, headers: &HeaderMap) -> bool {
            headers.contains_key("x-test-auth")
        }

        /// Counts the login, then fails or returns an `authorization: refreshed`
        /// header.
        fn login(&self, _: &str, deadline: Option<Instant>) -> Result<HeaderMap, String> {
            self.logins.fetch_add(1, Ordering::SeqCst);

            // Spend the browser's time on the test clock, returning by the
            // deadline as a provider must
            if let Some((tester, delay)) = &self.browser {
                let mut tester = tester.lock().unwrap();
                let now = tester.clock().now();
                tester.advance(deadline.map_or(*delay, |deadline| {
                    (*delay).min(deadline.saturating_duration_since(now))
                }));
            }
            if self.fail {
                return Err("test login refused".into());
            }
            Ok(HeaderMap::from_iter([(
                "authorization".parse().unwrap(),
                "refreshed".parse().unwrap(),
            )]))
        }
    }

    /// Formats an empty response carrying the `X-Test-Auth` header, which
    /// [`Login`] treats as refusing the caller's credentials.
    pub(in crate::cloud) fn refused(status: u16) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nX-Test-Auth: required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    /// Headers are looked up once, redacted from debug output and replaced by a
    /// login.
    #[test]
    fn credentials_are_cached_redacted_and_refreshed() {
        // Installing a provider looks nothing up
        let clock = test_clock().clock();
        let auth = Authorization::default();
        let login = Login::default();
        auth.set(Arc::new(login.clone()));
        assert_eq!(login.lookups.load(Ordering::SeqCst), 0);

        // Repeated requests reuse the first lookup, redacted
        let deadline = clock.now() + Duration::from_secs(1);
        for _ in 0..2 {
            let headers = auth.headers("https://test.invalid", deadline);
            assert_eq!(headers["authorization"], "cached");
            assert!(headers["authorization"].is_sensitive());
            assert!(!format!("{headers:?}").contains("cached"));
        }

        // A login replaces the cached headers, still redacted
        auth.login("https://test.invalid", &clock, deadline.into())
            .unwrap();
        let headers = auth.headers("https://test.invalid", deadline);
        assert_eq!(headers["authorization"], "refreshed");
        assert!(headers["authorization"].is_sensitive());
        assert!(!format!("{auth:?}").contains("refreshed"));
        assert_eq!(login.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(login.logins.load(Ordering::SeqCst), 1);
    }

    /// An absolute deadline ends a login with a timeout, while an inactivity
    /// allowance does not bound it.
    #[test]
    fn login_retains_absolute_deadlines() {
        // Let every browser login take 100 ms of the test clock
        let tester = Arc::new(Mutex::new(test_clock()));
        let clock = tester.lock().unwrap().clock();
        let auth = Authorization::default();
        let login = Login {
            browser: Some((tester, Duration::from_millis(100))),
            ..Default::default()
        };
        auth.set(Arc::new(login.clone()));

        // An expired deadline refuses before the browser opens
        assert!(matches!(
            auth.login("https://test.invalid", &clock, Timing::until(clock.now())),
            Err(Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        ));
        assert_eq!(login.logins.load(Ordering::SeqCst), 0);

        // An inactivity allowance does not bound the login, but an absolute
        // deadline does
        let timing = Timing::inactivity(Duration::from_millis(1));
        auth.login("https://test.invalid", &clock, timing).unwrap();
        assert!(matches!(
            auth.login(
                "https://test.invalid",
                &clock,
                timing.with_deadline(clock.now() + Duration::from_millis(5))
            ),
            Err(Failure::Wire(darkbio_wire::protocol::Error::Timeout))
        ));
    }
}
