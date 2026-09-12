// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! HTTP routes and payloads of the Ark cloud API.

use super::{Failure, auth};
use crate::schema::{CloudSyncFinishRequest, CloudSyncStartRequest};
use crate::trust::{Environment, Realm};
use crate::{Identity, Timing};
use base64::{
    Engine,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD},
};
use darkbio_wire::protocol;
use serde::{Deserialize, de::DeserializeOwned};
use std::time::Instant;

/// Maximum JSON response, enough for cloud certificates, signed time or registry state.
const MAX_RESPONSE: u64 = 64 * 1024;

/// Cloud operations selected by the attestation or an explicit environment.
#[derive(Debug)]
pub(super) struct Api {
    pub(super) auth: auth::Authorization, // Caller credentials, independent of the Ark proof
    pub(super) origin: String,            // HTTPS origin shared by API and socket credentials
    pub(super) agent: ureq::Agent,        // HTTP connections reused across the cloud exchange
    pub(super) url: String,               // API of the selected environment
    pub(super) realm: Realm,              // Realm selecting the device registry
    serial: Option<String>, // Attested serial, when available, checked against the registry
}

impl Api {
    /// Uses the selected environment and realm for relay attachment.
    pub(super) fn relay_url(&self) -> String {
        self.socket_url("relaying")
    }

    /// Uses the same realm for the companion pairing rendezvous.
    pub(super) fn pairing_url(&self) -> String {
        self.socket_url("pairing")
    }

    /// Converts the API origin to WebSocket and selects the realm-specific route.
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

    /// Prepares cloud access without I/O. An explicit environment overrides the
    /// attested one. Its discovery realm is used only without an attested realm.
    pub(super) fn new(identity: &Identity, cloud: Option<(Environment, Realm)>) -> Option<Self> {
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
            auth: auth::Authorization::default(),
            origin: api_url(*env).trim_end_matches("/v1").into(),
            agent: agent(),
            url: api_url(*env).into(),
            realm,
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

    /// Checks the registry with the Ark's opaque proof and matches its serial
    /// against the identity authenticated during the handshake, when attested.
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

    /// Retries only authentication or read-only requests after login. Callers
    /// recreate short-lived Ark proofs inside the attempt, after browser login.
    pub(super) fn with_auth<T>(
        &self,
        timing: Timing,
        mut attempt: impl FnMut() -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let result = attempt();
        if !matches!(result, Err(Failure::AuthRequired)) {
            return result;
        }
        self.auth.login(&self.origin, timing)?;
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
    /// Adds HTTP operation context without erasing a timeout or device error.
    fn context(self, context: &str) -> Self {
        match self {
            Self::Cloud(error) => Self::Cloud(format!("{context}: {error}")),
            error => error,
        }
    }
}

impl From<ureq::Error> for Failure {
    /// Keeps HTTP timeouts actionable without treating other failures as wire loss.
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
        Environment::Release => "https://api.dark.bio/v1",
        Environment::Staging => "https://api.darkbio.xyz/v1",
        Environment::Develop => "https://api.darkbio.dev/v1",
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
    /// Whether the registered device is currently permitted to use the cloud.
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

/// Sends the opaque proof to the registry of the authenticated realm.
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

/// Reads a proof-authenticated response, retaining refusal as a typed error.
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

/// Reads one successful JSON response under the remaining deadline and size limit.
pub(super) fn get<T: DeserializeOwned>(
    api: &Api,
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<T, Failure> {
    json(send(api, request, deadline)?)
}

/// Sends a GET under the remaining operation deadline without following redirects.
fn send(
    api: &Api,
    mut request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Failure> {
    for (name, value) in &api.auth.headers(&api.origin, deadline) {
        request = request.header(name, value);
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(protocol::Error::Timeout)?;
    let response = request
        .config()
        .timeout_global(Some(remaining))
        .build()
        .call()?;
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
    use crate::cloud::tests::{TIMEOUT, http, response, serve};
    use serde_json::json;
    use std::thread;
    use std::time::Duration;

    /// Redirects an attested connection to the loopback cloud.
    pub(in crate::cloud) fn api(url: String, realm: Realm) -> Api {
        Api {
            auth: auth::Authorization::default(),
            origin: url.trim_end_matches("/v1").into(),
            agent: http(),
            url,
            realm,
            serial: Some("test-serial".into()),
        }
    }

    /// Overrides select the cloud without replacing a verified realm or serial.
    /// Without attestation, the supplied discovery realm selects the registry.
    #[test]
    fn test_cloud_routing() {
        let key = darkbio_crypto::xdsa::SecretKey::generate().public_key();
        for &env in crate::identity::ENVIRONMENTS {
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
            let cloud = Api::new(&identity, None).unwrap();
            assert_eq!(cloud.url, api_url(env));
            assert_eq!(cloud.realm, Realm::Emulator);
            for &selected in crate::identity::ENVIRONMENTS {
                let cloud = Api::new(&identity, Some((selected, Realm::Hardware))).unwrap();
                assert_eq!(cloud.url, api_url(selected));
                assert_eq!(cloud.realm, Realm::Emulator);
                assert!(cloud.relay_url().ends_with("/sandbox/relaying"));
                assert_eq!(cloud.serial.as_deref(), Some("attested-serial"));
            }
            for identity in [
                Identity::SelfSigned(key.clone()),
                Identity::Recovered(key.clone()),
            ] {
                assert!(Api::new(&identity, None).is_none());
                for realm in [Realm::Hardware, Realm::Emulator] {
                    let cloud = Api::new(&identity, Some((env, realm))).unwrap();
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
        let cloud = api(url, Realm::Hardware);
        let deadline = Instant::now() + TIMEOUT;
        let start = fetch_identity(&cloud, deadline).unwrap();
        assert_eq!(start.signer, signer);
        assert_eq!(start.crypto, crypto);
        let finish = fetch_time(&cloud, &[0, 0xfb, 0xff], deadline).unwrap();
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
            let registration =
                fetch_registration(&api(url, realm), &[0xfb, 0xff], Instant::now() + TIMEOUT)
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
            assert!(fetch_identity(&api(url, Realm::Hardware), Instant::now() + TIMEOUT).is_err());
        }
        let (url, _requests) = serve(vec![(
            Duration::ZERO,
            response(200, r#"{"unixmilli":123,"signature":"!"}"#),
        )]);
        assert!(fetch_time(&api(url, Realm::Hardware), &[1], Instant::now() + TIMEOUT).is_err());
    }

    /// A later HTTP request retains the original deadline. A stalled response
    /// also expires instead of leaving setup waiting indefinitely.
    #[test]
    fn test_deadlines() {
        let body = r#"{"signer":"AA==","crypto":"AA=="}"#;
        let (url, _requests) = serve(vec![(Duration::ZERO, response(200, body))]);
        let cloud = api(url, Realm::Hardware);
        let deadline = Instant::now() + Duration::from_secs(1);
        fetch_identity(&cloud, deadline).unwrap();
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let error = fetch_time(&cloud, &[1], deadline).unwrap_err();
        assert!(matches!(error, Failure::Wire(protocol::Error::Timeout)));

        let (url, _requests) = serve(vec![(Duration::from_secs(1), response(200, body))]);
        assert!(matches!(
            fetch_identity(
                &api(url, Realm::Hardware),
                Instant::now() + Duration::from_millis(50)
            ),
            Err(Failure::Wire(protocol::Error::Timeout))
        ));
    }
}
