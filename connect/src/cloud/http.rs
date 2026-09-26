// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! HTTP routes and payloads of the Ark cloud API.

use super::{Failure, auth, dns};
use crate::schema::{CloudSyncFinishRequest, CloudSyncStartRequest};
use crate::trust::{Environment, Realm};
use crate::{Identity, Timing};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use darkbio_clock::Clock;
use darkbio_wire::protocol;
use serde::{Deserialize, de::DeserializeOwned};
use std::sync::Arc;
use std::time::Instant;

/// Largest JSON response body read, 64 KiB, enough for cloud certificates,
/// signed time or registry state.
const MAX_RESPONSE: u64 = 64 * 1024;

/// Cloud API client for the environment the attestation or the caller selects.
#[derive(Debug)]
pub(super) struct Api {
    /// Clock that the deadlines are measured on.
    pub(super) clock: Clock,

    /// Caller credentials, independent of the Ark's proof.
    pub(super) auth: auth::Authorization,
    /// HTTPS origin of the API, which the caller's credentials are kept for.
    pub(super) origin: String,
    /// HTTP client, reusing its connections across the cloud exchange.
    pub(super) agent: ureq::Agent,
    /// Base URL of the selected environment's API.
    pub(super) url: String,
    /// Realm selecting the registry and the socket routes.
    pub(super) realm: Realm,
    /// DNS lookups shared by this connection's cloud sockets.
    pub(super) resolver: Arc<dns::Resolver>,
    /// Serial from the attestation, when there is one, which the registry's
    /// answer must match.
    serial: Option<String>,
}

impl Api {
    /// Returns the WebSocket URL for relay attachment in the selected
    /// environment and realm.
    pub(super) fn relay_url(&self) -> String {
        self.socket_url("relaying")
    }

    /// Returns the WebSocket URL of the pairing rendezvous in the selected
    /// environment and realm.
    pub(super) fn pairing_url(&self) -> String {
        self.socket_url("pairing")
    }

    /// Converts the API URL to its WebSocket form and appends the route, under
    /// `sandbox/` for the emulator realm.
    fn socket_url(&self, route: &str) -> String {
        let url = self
            .url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        match self.realm {
            Realm::Hardware => format!("{url}/{route}"),
            Realm::Emulator => format!("{url}/sandbox/{route}"),
        }
    }

    /// Prepares cloud access without I/O, or returns `None` when neither the
    /// attestation nor the caller selects an environment.
    ///
    /// An explicit environment overrides the attested one. The caller's realm
    /// applies only when no attestation fixes it.
    pub(super) fn new(
        identity: &Identity,
        cloud: Option<(Environment, Realm)>,
        clock: &Clock,
    ) -> Option<Self> {
        let (env, realm, serial) = match identity {
            Identity::Attested { env, device } => (
                cloud.as_ref().map_or(env, |(env, _)| env),
                device.realm,
                Some(device.serial.clone()),
            ),
            Identity::SelfSigned(_) | Identity::Recovered(_) => {
                let (env, realm) = cloud.as_ref()?;
                (env, *realm, None)
            }
        };
        Some(Self {
            clock: clock.clone(),
            auth: auth::Authorization::default(),
            origin: api_url(*env).trim_end_matches("/v1").into(),
            agent: agent(),
            url: api_url(*env).into(),
            realm,
            resolver: dns::Resolver::new(clock),
            serial,
        })
    }

    /// Retrieves cloud certificates for the Ark to verify.
    pub(super) fn identity(&self, deadline: Instant) -> Result<CloudSyncStartRequest, Failure> {
        fetch_identity(self, deadline)
    }

    /// Retrieves signed time bound to the Ark's challenge.
    pub(super) fn time(
        &self,
        challenge: &[u8],
        deadline: Instant,
    ) -> Result<CloudSyncFinishRequest, Failure> {
        fetch_time(self, challenge, deadline)
    }

    /// Checks the registry with the Ark's opaque proof, requiring the registered
    /// serial to match an attested one.
    pub(super) fn genuine(&self, proof: &[u8], deadline: Instant) -> Result<Registration, Failure> {
        let registration = fetch_registration(self, proof, deadline)?;
        if self
            .serial
            .as_ref()
            .is_some_and(|serial| *serial != registration.serial)
        {
            return Err(Failure::Cloud(
                "cloud registry serial does not match the attested Ark".into(),
            ));
        }
        Ok(registration)
    }

    /// Runs a cloud step, logging in and running it once more when the host
    /// refuses the caller's credentials.
    ///
    /// Only authentication or read-only steps go through it, since a step can
    /// run twice. A step creates its Ark proof inside itself, so the run after
    /// a browser login carries a fresh one. A refusal after the login ends in
    /// [`Failure::CloudAuth`].
    pub(super) fn with_auth<T>(
        &self,
        timing: Timing,
        mut attempt: impl FnMut() -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let result = attempt();
        if !matches!(result, Err(Failure::AuthRequired)) {
            return result;
        }
        self.auth.login(&self.origin, &self.clock, timing)?;
        match attempt() {
            Err(Failure::AuthRequired) => Err(Failure::CloudAuth {
                origin: self.origin.clone(),
                message: "cloud access still refused after login".into(),
            }),
            result => result,
        }
    }
}

impl Failure {
    /// Prefixes a cloud failure with the operation that failed, leaving other
    /// kinds unchanged.
    fn context(self, context: &str) -> Self {
        match self {
            Self::Cloud(error) => Self::Cloud(format!("{context}: {error}")),
            error => error,
        }
    }
}

impl From<ureq::Error> for Failure {
    /// Maps an HTTP client timeout to a wire timeout and any other error to a
    /// cloud failure.
    fn from(error: ureq::Error) -> Self {
        match error {
            ureq::Error::Timeout(_) => Self::Wire(protocol::Error::Timeout),
            error => Self::Cloud(error.to_string()),
        }
    }
}

/// Returns the API base URL of an environment, the same for hardware and
/// emulators.
fn api_url(env: Environment) -> &'static str {
    match env {
        Environment::Release => "https://api.dark.bio/v1",
        Environment::Staging => "https://api.darkbio.xyz/v1",
        Environment::Develop => "https://api.darkbio.dev/v1",
    }
}

/// Builds the HTTP client for cloud operations, which never follows a redirect.
///
/// Each request gets the remaining operation budget as its timeout, and error
/// statuses come back as responses.
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
    /// CWT attesting the cloud's signing key.
    signer: String,
    /// CWT attesting the cloud's encryption key.
    crypto: String,
}

/// Cloud time and signature binding it to the Ark's challenge.
#[derive(Deserialize)]
struct SignedTime {
    /// Cloud time in Unix milliseconds, decoded as an integer.
    unixmilli: u64,
    /// Detached COSE signature, encoded as standard base64.
    signature: String,
}

/// Registry state of an Ark, as [`Client::genuine`](crate::Client::genuine)
/// returns it from the cloud.
///
/// A registered device may still be disabled, expired or superseded, which
/// [`Self::active`] sums up.
#[derive(Debug, Deserialize)]
pub struct Registration {
    /// Serial registered for the identity that produced the proof.
    pub serial: String,
    /// Attestation issuance time in Unix seconds.
    pub enrolled: i64,
    /// Whether the registry disabled this device.
    pub disabled: bool,
    /// Whether this emulator's attestation expired.
    pub expired: bool,
    /// Whether a newer emulator registration replaced this one.
    pub superseded: bool,
}

impl Registration {
    /// Checks whether the device is neither disabled, expired nor superseded.
    pub fn active(&self) -> bool {
        !self.disabled && !self.expired && !self.superseded
    }
}

/// Retrieves and decodes the cloud certificates without interpreting their claims.
fn fetch_identity(api: &Api, deadline: Instant) -> Result<CloudSyncStartRequest, Failure> {
    let certs: Certificates = get(
        api,
        api.agent.get(format!("{}/cloudsync/identity", api.url)),
        deadline,
    )
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
    api: &Api,
    challenge: &[u8],
    deadline: Instant,
) -> Result<CloudSyncFinishRequest, Failure> {
    let url = format!(
        "{}/cloudsync/time?challenge={}",
        api.url,
        hex::encode(challenge)
    );
    let time: SignedTime = get(api, api.agent.get(url), deadline)
        .map_err(|err| err.context("failed to fetch signed cloud time"))?;
    Ok(CloudSyncFinishRequest {
        unixmilli: time.unixmilli,
        signature: BASE64_STANDARD
            .decode(time.signature)
            .map_err(|err| format!("invalid cloud time signature encoding: {err}"))?,
    })
}

/// Sends the opaque proof to the registry of the connection's realm.
///
/// The proof travels in the `Dark-Auth` header as unpadded base64url.
fn fetch_registration(api: &Api, proof: &[u8], deadline: Instant) -> Result<Registration, Failure> {
    let route = match api.realm {
        Realm::Hardware => "genuine",
        Realm::Emulator => "sandbox/genuine",
    };
    get_authenticated(
        api,
        api.agent
            .get(format!("{}/{route}", api.url))
            .header("Dark-Auth", BASE64_URL_SAFE_NO_PAD.encode(proof)),
        deadline,
    )
    .map_err(|err| err.context("genuinity check failed"))
}

/// Sends a request carrying an Ark proof and decodes its JSON response,
/// reporting a 403 answer as [`Failure::ProofRejected`].
pub(super) fn get_authenticated<T: DeserializeOwned>(
    api: &Api,
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<T, Failure> {
    let response = send(api, request, deadline)?;
    if response.status() == 403 {
        return Err(Failure::ProofRejected);
    }
    json(response)
}

/// Sends a request and decodes its successful JSON response, within the
/// deadline and the response size limit.
pub(super) fn get<T: DeserializeOwned>(
    api: &Api,
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<T, Failure> {
    json(send(api, request, deadline)?)
}

/// Sends a GET with the caller's credentials under the remaining deadline,
/// without following redirects.
///
/// An expired deadline fails without sending. A response the caller's provider
/// recognizes as refusing its credentials becomes [`Failure::AuthRequired`].
fn send(
    api: &Api,
    mut request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Failure> {
    // Carry the caller's cached credentials
    for (name, value) in &api.auth.headers(&api.origin, deadline) {
        request = request.header(name, value);
    }

    // Bound the whole exchange by the time left on the deadline
    let remaining = deadline
        .checked_duration_since(api.clock.now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(protocol::Error::Timeout)?;
    let response = request
        .config()
        .timeout_global(Some(remaining))
        .build()
        .call()?;

    // Hand a refusal of the caller's credentials back for a login
    if api
        .auth
        .rejected(&api.origin, response.status(), response.headers())
    {
        return Err(Failure::AuthRequired);
    }
    Ok(response)
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
    use crate::cloud::tests::{TIMEOUT, http, response, serve, serve_inner};
    use crate::testing::test_clock;
    use serde_json::json;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Builds an API client for a loopback cloud, attested as `test-serial` and
    /// measuring its deadlines on the clock.
    pub(in crate::cloud) fn api(url: String, realm: Realm, clock: &Clock) -> Api {
        Api {
            clock: clock.clone(),
            auth: auth::Authorization::default(),
            origin: url.trim_end_matches("/v1").into(),
            agent: http(),
            url,
            realm,
            resolver: dns::Resolver::new(clock),
            serial: Some("test-serial".into()),
        }
    }

    /// An explicit environment selects the cloud without replacing an attested
    /// realm or serial, and only unattested identities take the caller's realm.
    #[test]
    fn test_cloud_routing() {
        let clock = test_clock().clock();
        let key = darkbio_crypto::xdsa::SecretKey::generate().public_key();
        for &env in crate::identity::ENVIRONMENTS {
            // The attestation alone selects its environment and realm
            let identity = Identity::Attested {
                env,
                device: crate::trust::device::Device {
                    realm: Realm::Emulator,
                    identity: key.clone(),
                    oem: darkbio_crypto::cwt::claims::eat::Oemid::new_pen(0),
                    serial: "attested-serial".into(),
                    model: vec![],
                    version: String::new(),
                    issued: 0,
                    expiry: Some(1),
                },
            };
            let cloud = Api::new(&identity, None, &clock).unwrap();
            assert_eq!(cloud.url, api_url(env));
            assert_eq!(cloud.realm, Realm::Emulator);

            // An explicit environment keeps the attested realm and serial
            for &selected in crate::identity::ENVIRONMENTS {
                let cloud = Api::new(&identity, Some((selected, Realm::Hardware)), &clock).unwrap();
                assert_eq!(cloud.url, api_url(selected));
                assert_eq!(cloud.realm, Realm::Emulator);
                assert!(cloud.relay_url().ends_with("/sandbox/relaying"));
                assert_eq!(cloud.serial.as_deref(), Some("attested-serial"));
            }

            // Unattested identities need an explicit route, whose realm applies
            for identity in [
                Identity::SelfSigned(key.clone()),
                Identity::Recovered(key.clone()),
            ] {
                assert!(Api::new(&identity, None, &clock).is_none());
                for realm in [Realm::Hardware, Realm::Emulator] {
                    let cloud = Api::new(&identity, Some((env, realm)), &clock).unwrap();
                    assert_eq!(cloud.url, api_url(env));
                    assert_eq!(cloud.realm, realm);
                    assert_eq!(cloud.serial, None);
                    assert_eq!(
                        cloud.relay_url().contains("/sandbox/"),
                        realm == Realm::Emulator
                    );
                    assert_eq!(identity.realm(), None);
                }
            }
        }
    }

    /// Sync payloads reach the Ark unchanged, with timestamps at integer
    /// precision and the challenge in the time route's query.
    #[test]
    fn test_sync_messages() {
        // Serve binary certificates and a timestamp beyond a double's precision
        let signer = [0, 0xff, 0xfb, 3];
        let crypto = [0xff, 0, 4, 5, 6];
        let signature = [0, 1, 0xfe, 0xff];
        let unixmilli = (1u64 << 53) + 1;
        let (url, requests) = serve(vec![
            response(
                200,
                &json!({
                    "signer": BASE64_STANDARD.encode(signer),
                    "crypto": BASE64_STANDARD.encode(crypto),
                })
                .to_string(),
            ),
            response(
                200,
                &json!({
                    "unixmilli": unixmilli,
                    "signature": BASE64_STANDARD.encode(signature),
                })
                .to_string(),
            ),
        ]);

        // Both payloads decode unchanged
        let clock = test_clock().clock();
        let cloud = api(url, Realm::Hardware, &clock);
        let deadline = clock.now() + TIMEOUT;
        let start = fetch_identity(&cloud, deadline).unwrap();
        assert_eq!(start.signer, signer);
        assert_eq!(start.crypto, crypto);
        let finish = fetch_time(&cloud, &[0, 0xfb, 0xff], deadline).unwrap();
        assert_eq!(finish.unixmilli, unixmilli);
        assert_eq!(finish.signature, signature);

        // Each request took its route, with the challenge hex encoded in the query
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /v1/cloudsync/identity HTTP/1.1\r\n")
        );
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /v1/cloudsync/time?challenge=00fbff HTTP/1.1\r\n")
        );
    }

    /// Each realm reaches its own registry with the proof in an unpadded
    /// base64url header, and inactive flags come back intact.
    #[test]
    fn test_registry_routes() {
        let clock = test_clock().clock();
        for (realm, path) in [
            (Realm::Hardware, "/v1/genuine"),
            (Realm::Emulator, "/v1/sandbox/genuine"),
        ] {
            // Serve a registration with every inactive flag set
            let (url, requests) = serve(vec![response(
                200,
                &json!({
                    "serial": "test-serial",
                    "enrolled": 123,
                    "disabled": true,
                    "expired": true,
                    "superseded": true,
                })
                .to_string(),
            )]);
            let registration = fetch_registration(
                &api(url, realm, &clock),
                &[0xfb, 0xff],
                clock.now() + TIMEOUT,
            )
            .unwrap();
            assert_eq!(registration.serial, "test-serial");
            assert_eq!(registration.enrolled, 123);
            assert!(registration.disabled && registration.expired && registration.superseded);
            assert!(!registration.active());

            // The request took the realm's route, with the proof in `Dark-Auth`
            let request = requests.recv().unwrap();
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
        let clock = test_clock().clock();
        let (url, _requests) = serve(vec![response(
            200,
            r#"{"serial":"another-ark","enrolled":123,"disabled":false,"expired":false,"superseded":false}"#,
        )]);
        assert!(matches!(
            api(url, Realm::Hardware, &clock).genuine(&[1], clock.now() + TIMEOUT),
            Err(Failure::Cloud(error)) if error.contains("serial does not match")
        ));
    }

    /// Invalid encodings, missing fields, HTTP refusals, redirects and oversized
    /// responses fail before a cloud payload can be forwarded to the Ark.
    #[test]
    fn test_bad_responses() {
        // Every broken identity response fails
        let clock = test_clock().clock();
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
            let (url, _requests) = serve(vec![response(status, &body)]);
            let cloud = api(url, Realm::Hardware, &clock);
            assert!(fetch_identity(&cloud, clock.now() + TIMEOUT).is_err());
        }

        // So does a signed time whose signature is not base64
        let (url, _requests) = serve(vec![response(200, r#"{"unixmilli":123,"signature":"!"}"#)]);
        let cloud = api(url, Realm::Hardware, &clock);
        assert!(fetch_time(&cloud, &[1], clock.now() + TIMEOUT).is_err());
    }

    /// A later request keeps the original deadline, and a stalled response
    /// expires instead of leaving setup waiting.
    #[test]
    fn test_deadlines() {
        // A request after the clock reached the shared deadline fails without HTTP
        let mut tester = test_clock();
        let clock = tester.clock();
        let body = r#"{"signer":"AA==","crypto":"AA=="}"#;
        let (url, _requests) = serve(vec![response(200, body)]);
        let cloud = api(url, Realm::Hardware, &clock);
        let deadline = clock.now() + Duration::from_secs(1);
        fetch_identity(&cloud, deadline).unwrap();
        tester.advance_to(deadline);
        let error = fetch_time(&cloud, &[1], deadline).unwrap_err();
        assert!(matches!(error, Failure::Wire(protocol::Error::Timeout)));

        // A response the server never sends ends at the HTTP client's own timeout
        let (_release, pause) = mpsc::channel();
        let (url, _requests) = serve_inner(vec![response(200, body)], Some(pause));
        assert!(matches!(
            fetch_identity(
                &api(url, Realm::Hardware, &clock),
                clock.now() + Duration::from_millis(50)
            ),
            Err(Failure::Wire(protocol::Error::Timeout))
        ));
    }
}
