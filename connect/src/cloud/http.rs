// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! HTTP routes and payloads of the Ark cloud API.

use super::Failure;
use crate::schema::{CloudSyncFinishRequest, CloudSyncStartRequest};
use crate::trust::{Environment, Realm};
use crate::{Error, Identity};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use darkbio_wire::protocol;
use serde::{Deserialize, de::DeserializeOwned};
use std::time::Instant;

type Authenticate =
    dyn Fn(&str, Option<&str>, Instant) -> Result<Option<(String, String)>, Error> + Send + Sync;

/// Caller-owned authentication for the package origin. Credentials never enter
/// the agent's defaults, where cloud API requests could inherit them.
pub(crate) struct PackageAuth(Box<Authenticate>);

impl PackageAuth {
    pub(crate) fn new(
        auth: impl Fn(&str, Option<&str>, Instant) -> Result<Option<(String, String)>, Error>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self(Box::new(auth))
    }
}

impl std::fmt::Debug for PackageAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PackageAuth")
    }
}

/// Maximum JSON response, enough for cloud certificates, signed time or registry state.
const MAX_RESPONSE: u64 = 64 * 1024;

/// Cloud operations selected by the identity established during the handshake.
#[derive(Debug)]
pub(super) struct Api {
    pub(super) agent: ureq::Agent, // HTTP connections reused across the cloud exchange
    pub(super) url: String,        // API of the environment verified during the handshake
    pub(super) packages: String,   // Package repository of the same verified environment
    pub(super) realm: Realm,       // Verified realm selecting the device registry
    serial: String,                // Attested serial expected in the registry response
}

impl Api {
    /// Retries a package GET once with caller-supplied authentication. The login
    /// redirect is only a challenge; the retry always uses the original URL.
    pub(super) fn package(
        &self,
        path: &str,
        auth: Option<&PackageAuth>,
        deadline: Instant,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let request = |header: Option<(String, String)>| {
            let mut request = self.agent.get(format!("{}/{path}", self.packages));
            if let Some((name, value)) = header {
                let mut value = ureq::http::HeaderValue::from_str(&value)
                    .map_err(|_| Failure::Cloud("invalid package authentication header".into()))?;
                value.set_sensitive(true);
                request = request.header(name, value);
            }
            send(request, deadline)
        };
        let header = match auth {
            Some(auth) => (auth.0)(&self.packages, None, deadline)?,
            None => None,
        };
        let response = request(header)?;
        if response.status().is_redirection()
            && let Some(auth) = auth
            && let Some(redirect) = response
                .headers()
                .get("Location")
                .and_then(|value| value.to_str().ok())
            && let Some(header) = (auth.0)(&self.packages, Some(redirect), deadline)?
        {
            drop(response);
            return Ok(request(Some(header))?);
        }
        Ok(response)
    }

    /// Uses the authenticated environment and realm for relay attachment.
    pub(super) fn relay_url(&self) -> String {
        let url = self
            .url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        match self.realm {
            Realm::Hardware => format!("{url}/relaying"),
            Realm::Emulator => format!("{url}/sandbox/relaying"),
        }
    }

    /// Prepares cloud access without I/O. Self-signed and recovery connections
    /// have no authenticated environment or realm and skip cloud setup.
    pub(super) fn new(identity: &Identity) -> Option<Self> {
        let Identity::Attested { env, device } = identity else {
            return None;
        };
        Some(Self {
            agent: agent(),
            url: api_url(*env).into(),
            packages: package_url(*env).into(),
            realm: device.realm,
            serial: device.serial.clone(),
        })
    }

    /// Retrieves cloud certificates for the Ark to verify.
    pub(super) fn identity(&self, deadline: Instant) -> Result<CloudSyncStartRequest, Failure> {
        fetch_identity(&self.agent, &self.url, deadline)
    }

    /// Retrieves signed time bound to the Ark's challenge.
    pub(super) fn time(
        &self,
        challenge: &[u8],
        deadline: Instant,
    ) -> Result<CloudSyncFinishRequest, Failure> {
        fetch_time(&self.agent, &self.url, challenge, deadline)
    }

    /// Checks the registry with the Ark's opaque proof and matches its serial
    /// against the identity authenticated during the handshake.
    pub(super) fn genuine(&self, proof: &[u8], deadline: Instant) -> Result<Registration, Failure> {
        let registration = fetch_registration(&self.agent, &self.url, self.realm, proof, deadline)?;
        if registration.serial != self.serial {
            return Err(Failure::Cloud(
                "cloud registry serial does not match the attested Ark".into(),
            ));
        }
        Ok(registration)
    }
}

impl Failure {
    /// Adds HTTP operation context without erasing a timeout or device error.
    fn context(self, context: &str) -> Self {
        match self {
            Self::Cloud(error) => Self::Cloud(format!("{context}: {error}")),
            error => error,
        }
    }
}

impl From<ureq::Error> for Failure {
    fn from(error: ureq::Error) -> Self {
        match error {
            ureq::Error::Timeout(_) => Self::Wire(protocol::Error::Timeout),
            error => Self::Cloud(error.to_string()),
        }
    }
}

/// Cloud sync serves hardware and emulators through the same environment routes.
fn api_url(env: Environment) -> &'static str {
    match env {
        #[cfg(feature = "release")]
        Environment::Release => "https://api.dark.bio/v1",
        #[cfg(feature = "staging")]
        Environment::Staging => "https://api.darkbio.xyz/v1",
        #[cfg(feature = "develop")]
        Environment::Develop => "https://api.darkbio.dev/v1",
    }
}

/// Firmware archives live on the package host of the authenticated environment.
fn package_url(env: Environment) -> &'static str {
    match env {
        #[cfg(feature = "release")]
        Environment::Release => "https://pkg.dark.bio",
        #[cfg(feature = "staging")]
        Environment::Staging => "https://pkg.darkbio.xyz",
        #[cfg(feature = "develop")]
        Environment::Develop => "https://pkg.darkbio.dev",
    }
}

/// HTTP client for cloud operations. Each request receives the remaining
/// operation budget; redirects are refused rather than changing the endpoint.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// Cloud attestations encoded as standard base64 by the identity route.
#[derive(Deserialize)]
struct Certificates {
    signer: String, // CWT attesting the cloud signing key
    crypto: String, // CWT attesting the cloud encryption key
}

/// Cloud time and signature binding it to the Ark's challenge.
#[derive(Deserialize)]
struct SignedTime {
    unixmilli: u64,    // Unix timestamp in milliseconds, decoded without floating point
    signature: String, // Detached COSE signature encoded as standard base64
}

/// Registry state returned after the cloud verifies the Ark's proof. A registered
/// device may still be disabled, expired or superseded.
#[derive(Debug, Deserialize)]
pub struct Registration {
    pub serial: String,   // Serial of the registered device
    pub enrolled: i64,    // Attestation issuance time in Unix seconds
    pub disabled: bool,   // Whether the registry disabled the device
    pub expired: bool,    // Whether an emulator attestation expired
    pub superseded: bool, // Whether another emulator replaced this one
}

impl Registration {
    /// Whether the registered device is currently permitted to use the cloud.
    pub fn active(&self) -> bool {
        !self.disabled && !self.expired && !self.superseded
    }
}

/// Retrieves and decodes the cloud certificates without interpreting their claims.
fn fetch_identity(
    agent: &ureq::Agent,
    url: &str,
    deadline: Instant,
) -> Result<CloudSyncStartRequest, Failure> {
    let certs: Certificates = get(agent.get(format!("{url}/cloudsync/identity")), deadline)
        .map_err(|err| err.context("failed to fetch cloud identity"))?;
    Ok(CloudSyncStartRequest {
        signer: BASE64_STANDARD
            .decode(certs.signer)
            .map_err(|err| format!("invalid cloud signing certificate encoding: {err}"))?,
        crypto: BASE64_STANDARD
            .decode(certs.crypto)
            .map_err(|err| format!("invalid cloud encryption certificate encoding: {err}"))?,
    })
}

/// Sends the Ark's challenge as hex and decodes the cloud's signed response.
fn fetch_time(
    agent: &ureq::Agent,
    url: &str,
    challenge: &[u8],
    deadline: Instant,
) -> Result<CloudSyncFinishRequest, Failure> {
    let url = format!("{url}/cloudsync/time?challenge={}", hex::encode(challenge));
    let time: SignedTime = get(agent.get(url), deadline)
        .map_err(|err| err.context("failed to fetch signed cloud time"))?;
    Ok(CloudSyncFinishRequest {
        unixmilli: time.unixmilli,
        signature: BASE64_STANDARD
            .decode(time.signature)
            .map_err(|err| format!("invalid cloud time signature encoding: {err}"))?,
    })
}

/// Sends the opaque proof to the registry of the authenticated realm.
fn fetch_registration(
    agent: &ureq::Agent,
    url: &str,
    realm: Realm,
    proof: &[u8],
    deadline: Instant,
) -> Result<Registration, Failure> {
    let route = match realm {
        Realm::Hardware => "genuine",
        Realm::Emulator => "sandbox/genuine",
    };
    get(
        agent
            .get(format!("{url}/{route}"))
            .header("Dark-Auth", BASE64_URL_SAFE_NO_PAD.encode(proof)),
        deadline,
    )
    .map_err(|err| err.context("genuinity check failed"))
}

/// Reads one successful JSON response under the remaining deadline and size limit.
pub(super) fn get<T: DeserializeOwned>(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<T, Failure> {
    json(send(request, deadline)?)
}

/// Sends a GET under the remaining operation deadline without following redirects.
fn send(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Failure> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(protocol::Error::Timeout)?;
    Ok(request
        .config()
        .timeout_global(Some(remaining))
        .build()
        .call()?)
}

/// Decodes a successful JSON response within the cloud response size limit.
pub(super) fn json<T: DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, Failure> {
    if !response.status().is_success() {
        return Err(Failure::Cloud(format!("HTTP {}", response.status())));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE)
        .read_to_vec()?;
    serde_json::from_slice(&body).map_err(|err| Failure::Cloud(err.to_string()))
}

/// Cloud HTTP contracts, deadline handling and registry state.
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::cloud::tests::{TIMEOUT, http, response, serve};
    use serde_json::json;
    use std::thread;
    use std::time::Duration;

    /// Redirects an attested connection to the loopback cloud.
    pub(in crate::cloud) fn api(url: String, realm: Realm) -> Api {
        Api {
            agent: http(),
            packages: url.trim_end_matches("/v1").to_owned(),
            url,
            realm,
            serial: "test-serial".into(),
        }
    }

    /// Login retries stay on the package origin. Cached credentials reach the
    /// archive, while cloud API requests through the same agent remain separate.
    #[test]
    fn test_package_auth() {
        let redirect = "https://login.invalid/session?secret=redacted";
        let (url, requests) = serve(vec![
            (
                Duration::ZERO,
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            ),
            (Duration::ZERO, response(200, "catalog")),
            (Duration::ZERO, response(200, "archive")),
            (
                Duration::ZERO,
                response(200, r#"{"signer":"AQ==","crypto":"Ag=="}"#),
            ),
        ]);
        let cloud = api(url, Realm::Hardware);
        let origin = cloud.packages.clone();
        let cached = std::sync::Mutex::new(false);
        let auth = PackageAuth::new(move |host, challenge, _| {
            assert_eq!(host, origin);
            let mut cached = cached.lock().unwrap();
            if let Some(challenge) = challenge {
                assert_eq!(challenge, redirect);
                assert!(!*cached);
                *cached = true;
            }
            Ok(cached.then(|| ("test-auth".into(), "private-token".into())))
        });
        let deadline = Instant::now() + TIMEOUT;
        for path in ["imgs/arkos.pkgs", "imgs/archive.arch"] {
            assert_eq!(
                cloud.package(path, Some(&auth), deadline).unwrap().status(),
                200
            );
        }
        cloud.identity(deadline).unwrap();
        for (path, authenticated) in [
            ("/imgs/arkos.pkgs", false),
            ("/imgs/arkos.pkgs", true),
            ("/imgs/archive.arch", true),
            ("/v1/cloudsync/identity", false),
        ] {
            let request = requests.recv_timeout(TIMEOUT).unwrap().to_ascii_lowercase();
            assert!(request.starts_with(&format!("get {path} http/1.1\r\n")));
            assert_eq!(
                request.contains("test-auth: private-token\r\n"),
                authenticated
            );
        }
        assert_eq!(format!("{auth:?}"), "PackageAuth");
    }

    /// An ignored or repeated redirect stops the request. Authentication cannot
    /// extend the deadline or trigger a chain of retries.
    #[test]
    fn test_package_auth_failure() {
        for mode in ["decline", "repeat", "expire", "error"] {
            let redirect = "HTTP/1.1 302 Found\r\nLocation: https://login.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let count = if mode == "repeat" { 2 } else { 1 };
            let (url, requests) = serve(vec![(Duration::ZERO, redirect.into()); count]);
            let cloud = api(url, Realm::Hardware);
            let auth = PackageAuth::new(move |_, redirect, deadline| {
                if redirect.is_none() || mode == "decline" {
                    return Ok(None);
                }
                if mode == "error" {
                    return Err(Error::Cloud("login refused".into()));
                }
                if mode == "expire" {
                    thread::sleep(deadline.saturating_duration_since(Instant::now()));
                }
                Ok(Some(("test-auth".into(), "private-token".into())))
            });
            let result = cloud.package(
                "imgs/arkos.pkgs",
                Some(&auth),
                Instant::now() + Duration::from_millis(100),
            );
            match mode {
                "expire" => assert!(matches!(result, Err(Error::Timeout))),
                "error" => {
                    assert!(matches!(result, Err(Error::Cloud(error)) if error == "login refused"))
                }
                _ => assert_eq!(result.unwrap().status(), 302),
            }
            for _ in 0..count {
                requests.recv_timeout(TIMEOUT).unwrap();
            }
            assert!(requests.recv_timeout(TIMEOUT).is_err());
        }
    }

    /// Certificates and signatures reach the Ark byte-for-byte. Timestamps retain
    /// integer precision, and the challenge is encoded in the time route's query.
    #[test]
    fn test_sync_messages() {
        let signer = [0, 0xff, 0xfb, 3];
        let crypto = [0xff, 0, 4, 5, 6];
        let signature = [0, 1, 0xfe, 0xff];
        let unixmilli = (1u64 << 53) + 1;
        let (url, requests) = serve(vec![
            (
                Duration::ZERO,
                response(
                    200,
                    &json!({
                        "signer": BASE64_STANDARD.encode(signer),
                        "crypto": BASE64_STANDARD.encode(crypto),
                    })
                    .to_string(),
                ),
            ),
            (
                Duration::ZERO,
                response(
                    200,
                    &json!({
                        "unixmilli": unixmilli,
                        "signature": BASE64_STANDARD.encode(signature),
                    })
                    .to_string(),
                ),
            ),
        ]);
        let agent = http();
        let deadline = Instant::now() + TIMEOUT;
        let start = fetch_identity(&agent, &url, deadline).unwrap();
        assert_eq!(start.signer, signer);
        assert_eq!(start.crypto, crypto);
        let finish = fetch_time(&agent, &url, &[0, 0xfb, 0xff], deadline).unwrap();
        assert_eq!(finish.unixmilli, unixmilli);
        assert_eq!(finish.signature, signature);
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .starts_with("GET /v1/cloudsync/identity HTTP/1.1\r\n")
        );
        assert!(
            requests
                .recv_timeout(TIMEOUT)
                .unwrap()
                .starts_with("GET /v1/cloudsync/time?challenge=00fbff HTTP/1.1\r\n")
        );
    }

    /// The attested realm selects the registry, and the proof travels in an
    /// unpadded URL-safe authentication header. Inactive flags are retained.
    #[test]
    fn test_registry_routes() {
        for (realm, path) in [
            (Realm::Hardware, "/v1/genuine"),
            (Realm::Emulator, "/v1/sandbox/genuine"),
        ] {
            let (url, requests) = serve(vec![(
                Duration::ZERO,
                response(
                    200,
                    &json!({
                        "serial": "test-serial",
                        "enrolled": 123,
                        "disabled": true,
                        "expired": true,
                        "superseded": true,
                    })
                    .to_string(),
                ),
            )]);
            let registration = fetch_registration(
                &http(),
                &url,
                realm,
                &[0xfb, 0xff],
                Instant::now() + TIMEOUT,
            )
            .unwrap();
            assert_eq!(registration.serial, "test-serial");
            assert_eq!(registration.enrolled, 123);
            assert!(registration.disabled && registration.expired && registration.superseded);
            assert!(!registration.active());
            let request = requests.recv_timeout(TIMEOUT).unwrap();
            assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\ndark-auth: -_8\r\n")
            );
        }
    }

    /// A registry reply for another serial cannot verify this connection.
    #[test]
    fn test_registry_identity() {
        let (url, _requests) = serve(vec![(
            Duration::ZERO,
            response(
                200,
                r#"{"serial":"another-ark","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#,
            ),
        )]);
        assert!(matches!(
            api(url, Realm::Hardware).genuine(&[1], Instant::now() + TIMEOUT),
            Err(Failure::Cloud(error)) if error.contains("serial does not match")
        ));
    }

    /// Invalid encodings, missing fields, HTTP refusals, redirects and oversized
    /// responses fail before a cloud payload can be forwarded to the Ark.
    #[test]
    fn test_bad_responses() {
        for (status, body) in [
            (503, "unavailable".into()),
            (302, "redirect".into()),
            (200, "{broken".into()),
            (200, "{}".into()),
            (200, r#"{"signer":"!","crypto":"AA=="}"#.into()),
            (200, r#"{"signer":"AA==","crypto":"!"}"#.into()),
            (
                200,
                json!({
                    "signer": "A".repeat(MAX_RESPONSE as usize),
                    "crypto": "AA==",
                })
                .to_string(),
            ),
        ] {
            let (url, _requests) = serve(vec![(Duration::ZERO, response(status, &body))]);
            assert!(fetch_identity(&http(), &url, Instant::now() + TIMEOUT).is_err());
        }
        let (url, _requests) = serve(vec![(
            Duration::ZERO,
            response(200, r#"{"unixmilli":123,"signature":"!"}"#),
        )]);
        assert!(fetch_time(&http(), &url, &[1], Instant::now() + TIMEOUT).is_err());
    }

    /// A later HTTP request retains the original deadline. A stalled response
    /// also expires instead of leaving setup waiting indefinitely.
    #[test]
    fn test_deadlines() {
        let body = r#"{"signer":"AA==","crypto":"AA=="}"#;
        let (url, _requests) = serve(vec![(Duration::ZERO, response(200, body))]);
        let agent = http();
        let deadline = Instant::now() + Duration::from_secs(1);
        fetch_identity(&agent, &url, deadline).unwrap();
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let error = fetch_time(&agent, &url, &[1], deadline).unwrap_err();
        assert!(matches!(error, Failure::Wire(protocol::Error::Timeout)));

        let (url, _requests) = serve(vec![(Duration::from_secs(1), response(200, body))]);
        assert!(matches!(
            fetch_identity(&agent, &url, Instant::now() + Duration::from_millis(50)),
            Err(Failure::Wire(protocol::Error::Timeout))
        ));
    }
}
