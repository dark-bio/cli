// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Device authentication through wire's verifier and the enabled roots of trust.
//! The returned identity records whether the peer was attested, self-signed or pinned.

use darkbio_crypto::xdsa;
use darkbio_trust as trust;
use darkbio_trust::Environment;
use darkbio_trust::device::Device;
use darkbio_wire::transport::{Attestation, Verifier};
use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

/// Known environments. Disabled features leave their root sets empty.
pub(crate) const ENVIRONMENTS: &[Environment] = &[
    Environment::Release,
    Environment::Staging,
    Environment::Develop,
];

/// Controls which device attestations are accepted during the handshake.
pub enum TrustMode {
    /// Accept Arks attested under the hardware or emulator roots of any
    /// environment the build trusts, or peers presenting a self-signed
    /// attestation. Self-signing proves key possession, not provisioning history.
    RootOrSelf,

    /// Skip the attestation and authenticate the handshake against the given
    /// identity key instead, recovering Arks with a corrupted or missing one.
    Recover(Box<xdsa::PublicKey>),
}

/// Peer identity established by the handshake and the selected trust policy.
#[derive(Clone)]
pub enum Identity {
    /// Attested under the roots of an environment.
    Attested {
        env: Environment, // Environment whose root signed the attestation
        device: Device,   // Verified device details from the attestation
    },

    /// Self-attested key possession. Provisioning and genuineness are unverified.
    SelfSigned(xdsa::PublicKey),

    /// Pinned by the caller, the attestation was not checked.
    Recovered(xdsa::PublicKey),
}

impl Identity {
    /// Returns the realm established by a trusted attestation. Self-signed and
    /// recovered identities have no verified realm, regardless of their transport.
    pub fn realm(&self) -> Option<trust::Realm> {
        match self {
            Self::Attested { device, .. } => Some(device.realm),
            Self::SelfSigned(_) | Self::Recovered(_) => None,
        }
    }

    /// Returns the identity key used to authenticate the handshake.
    pub fn key(&self) -> &xdsa::PublicKey {
        match self {
            Identity::Attested { device, .. } => &device.identity,
            Identity::SelfSigned(key) | Identity::Recovered(key) => key,
        }
    }
}

impl Verifier for TrustMode {
    type Info = Identity;

    /// Verifies the attestation or returns the pinned key selected for recovery.
    fn verify(&self, attestation: &Attestation) -> Result<(xdsa::PublicKey, Identity), String> {
        self.verify_identity(attestation, &Cell::new(None))
    }
}

impl TrustMode {
    fn verify_identity(
        &self,
        attestation: &Attestation,
        excluded: &Cell<Option<Environment>>,
    ) -> Result<(xdsa::PublicKey, Identity), String> {
        // Recovery authenticates key possession without consulting the attestation.
        if let TrustMode::Recover(key) = self {
            return Ok((*key.clone(), Identity::Recovered(*key.clone())));
        }
        // Look the signer up in the roots of every environment. An attestation
        // from a known root that fails to verify is a hard error, only unknown
        // signers fall through to the self-signed check. Retain their diagnostic
        // so a missing root is not obscured by the self-signed fallback.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_secs();

        let mut untrusted = None;
        for &env in ENVIRONMENTS {
            let hardware = trust::roots::hardware(env);
            let emulator = trust::roots::emulator(env);
            match trust::device::verify(attestation.as_bytes(), hardware, emulator, Some(now)) {
                Ok(device) => {
                    return Ok((device.identity.clone(), Identity::Attested { env, device }));
                }
                Err(err @ trust::Error::UntrustedSigner { .. }) => untrusted = Some(err),
                Err(err) => return Err(err.to_string()),
            }
        }
        // No trusted root matched. Only an attestation signed by its own identity
        // key can establish a self-signed peer.
        let key =
            trust::device::verify_self_signed(attestation.as_bytes()).map_err(|err| {
                match (err, untrusted) {
                    (trust::Error::NotSelfSigned, Some(err)) => signer_error(err, excluded),
                    (err, _) => err.to_string(),
                }
            })?;
        Ok((key.clone(), Identity::SelfSigned(key)))
    }
}

/// Gives the required build feature for an excluded device root.
fn signer_error(error: trust::Error, excluded: &Cell<Option<Environment>>) -> String {
    if let trust::Error::UntrustedSigner {
        root: Some(root), ..
    } = &error
        && matches!(
            root.role,
            trust::roots::Role::DeviceAttester | trust::roots::Role::EmulatorAttester
        )
        && trust::roots::hardware(root.env).is_empty()
        && trust::roots::emulator(root.env).is_empty()
    {
        excluded.set(Some(root.env));
        return format!(
            "{} support is disabled; rebuild with --features {}",
            root.env, root.env,
        );
    }
    error.to_string()
}

/// Retains the trust outcome across wire's string-only verifier error boundary.
pub(crate) struct Verification<'a> {
    policy: &'a TrustMode,
    excluded: Cell<Option<Environment>>,
}

impl<'a> Verification<'a> {
    pub(crate) fn new(policy: &'a TrustMode) -> Self {
        Self {
            policy,
            excluded: Cell::new(None),
        }
    }

    pub(crate) fn error(&self, err: crate::Error) -> crate::Error {
        self.excluded.get().map_or(err, crate::Error::Untrusted)
    }
}

impl Verifier for Verification<'_> {
    type Info = Identity;

    fn verify(&self, attestation: &Attestation) -> Result<(xdsa::PublicKey, Identity), String> {
        self.excluded.set(None);
        self.policy.verify_identity(attestation, &self.excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::self_attestation;
    use darkbio_crypto::{cbor, cose};

    // Tests that the root trust mode accepts a self-signed attestation with the
    // Ark's own identity, refuses one signed by an unknown key, and that the
    // recovery mode pins the given identity regardless of the attestation.
    #[test]
    fn test_trust_modes() {
        let identity = xdsa::SecretKey::generate();
        let foreign = xdsa::SecretKey::generate();

        // Self-signed attestation proves possession of its key only.
        let attestation = self_attestation(&identity, identity.public_key());
        let (key, info) = TrustMode::RootOrSelf.verify(&attestation).unwrap();
        assert_eq!(key.fingerprint(), identity.public_key().fingerprint());
        assert!(matches!(info, Identity::SelfSigned(_)));
        assert_eq!(info.realm(), None);

        // Attestation by an unknown key, refused
        let attestation = self_attestation(&foreign, identity.public_key());
        let error = TrustMode::RootOrSelf.verify(&attestation).err().unwrap();
        assert!(error.contains(&hex::encode(foreign.fingerprint().to_bytes())));
        assert!(error.contains("unknown key"));
        assert!(!error.contains("--features"));

        // Recovery mode, the pinned key is used regardless of the attestation
        let pinned = xdsa::SecretKey::generate().public_key();
        let (key, info) = TrustMode::Recover(Box::new(pinned.clone()))
            .verify(&attestation)
            .unwrap();
        assert_eq!(key.fingerprint(), pinned.fingerprint());
        assert!(matches!(info, Identity::Recovered(_)));
        assert_eq!(info.realm(), None);
    }

    /// A claimed known signer identifies the missing feature. Enabling that root must
    /// verify the signature instead of accepting the claimed fingerprint.
    #[test]
    fn test_known_signer() {
        let key = xdsa::SecretKey::generate();
        let attestation = self_attestation(&key, key.public_key());
        for fingerprint in [
            "8b842c20bb8083a1635140e58675f3b95a100ac0e39ab82fa6cb2ef23eb532fb", // Release hardware
            "7d725c5cb3f80ef4e17bb98ea1f14683714a7eaffaa78cded17ff0e2c0c96ffa", // Staging hardware
            "456df8b670cbe2c1c95368f2678ddf66671826542d742cfb5aee2f30e194b2ed", // Develop hardware
            "4ae00e993329f6f46e350b247a8e2b38b915ade524ceca70d6530c859d1f69ef", // Develop emulator
            "9dfa577f0938f11f9df9a5eadcdc8e57353702dca6d23291cf5cc71a403d67a8", // Develop cloud
        ] {
            let bytes: [u8; 32] = hex::decode(fingerprint).unwrap().try_into().unwrap();
            let claimed = xdsa::Fingerprint::from_bytes(&bytes);
            let root = trust::roots::identify(&claimed).unwrap();
            let mut envelope: cose::CoseSign1 = cbor::decode(attestation.as_bytes()).unwrap();
            let mut header: cose::SigProtectedHeader = cbor::decode(&envelope.protected).unwrap();
            header.kid = claimed;
            envelope.protected = cbor::encode(&header).unwrap();
            let forged = Attestation::new(cbor::encode(&envelope).unwrap()).unwrap();
            let error = TrustMode::RootOrSelf.verify(&forged).err().unwrap();
            let verifier = Verification::new(&TrustMode::RootOrSelf);
            assert!(verifier.verify(&forged).is_err());
            let typed = verifier.error(crate::Error::Closed);

            let trusted = match root.role {
                trust::roots::Role::DeviceAttester => !trust::roots::hardware(root.env).is_empty(),
                trust::roots::Role::EmulatorAttester => {
                    !trust::roots::emulator(root.env).is_empty()
                }
                _ => false,
            };
            if trusted {
                assert!(matches!(typed, crate::Error::Closed));
                assert!(error.starts_with("cwt:"), "{error}");
                assert!(!error.contains("--features"));
            } else {
                if root.role == trust::roots::Role::CloudAttester {
                    assert!(matches!(typed, crate::Error::Closed));
                    assert!(error.contains(fingerprint), "{error}");
                    assert!(error.contains(&root.to_string()), "{error}");
                    assert!(!error.contains("--features"));
                } else {
                    assert!(matches!(typed, crate::Error::Untrusted(env) if env == root.env));
                    assert_eq!(
                        error,
                        format!(
                            "{} support is disabled; rebuild with --features {}",
                            root.env, root.env
                        ),
                    );
                }
            }
        }
    }

    /// Invalid self-signatures retain their cryptographic error instead of being
    /// mislabeled as excluded roots.
    #[test]
    fn test_invalid_attestation() {
        let key = xdsa::SecretKey::generate();
        let attestation = self_attestation(&key, key.public_key());
        let mut bytes = attestation.as_bytes().to_vec();
        *bytes.last_mut().unwrap() ^= 1;
        let attestation = Attestation::new(bytes).unwrap();
        let error = TrustMode::RootOrSelf.verify(&attestation).err().unwrap();
        assert!(error.starts_with("cwt:"), "{error}");
        assert!(!error.contains("--features"));
        assert!(!error.contains("not among the trusted roots"));
    }
}
