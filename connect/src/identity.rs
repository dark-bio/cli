// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Device authentication through wire's verifier and the CLI's roots of trust.
//!
//! The returned identity records whether the peer was attested, self-signed or
//! pinned.

use darkbio_crypto::xdsa;
use darkbio_trust as trust;
use darkbio_trust::Environment;
use darkbio_trust::device::Device;
use darkbio_wire::transport::{Attestation, Verifier};
use std::time::{SystemTime, UNIX_EPOCH};

/// Environments whose roots the CLI trusts.
pub(crate) const ENVIRONMENTS: &[Environment] = &[
    Environment::Release,
    Environment::Staging,
    Environment::Develop,
];

/// Policy for which device attestations the handshake accepts.
pub enum TrustMode {
    /// Policy accepting Arks attested under the release, staging or develop
    /// hardware or emulator roots, or peers presenting a self-signed
    /// attestation.
    ///
    /// Self-signing proves key possession, not provisioning history.
    RootOrSelf,

    /// Policy authenticating the handshake against the given identity key
    /// instead of the attestation, for recovering Arks whose attestation is
    /// corrupted or missing.
    Recover(Box<xdsa::PublicKey>),
}

/// Peer identity established by the handshake and the selected trust policy.
#[derive(Clone)]
pub enum Identity {
    /// Peer attested under the roots of an environment.
    Attested {
        /// Environment whose root verified the device attestation.
        env: Environment,
        /// Signed identity and provisioning details, independent of cloud status.
        device: Device,
    },

    /// Peer proving possession of its self-attested key.
    ///
    /// Provisioning and genuineness are unverified.
    SelfSigned(xdsa::PublicKey),

    /// Peer authenticated against a key the caller pinned, its attestation
    /// unchecked.
    Recovered(xdsa::PublicKey),
}

impl Identity {
    /// Returns the realm established by a trusted attestation.
    ///
    /// Self-signed and recovered identities have no verified realm, regardless
    /// of their transport.
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
    /// Trust outcome returned alongside the authenticated handshake key.
    type Info = Identity;

    /// Verifies the attestation at `now` or returns the pinned key selected for recovery.
    fn verify(
        &self,
        attestation: &Attestation,
        now: SystemTime,
    ) -> Result<(xdsa::PublicKey, Identity), String> {
        // Recovery proves possession of the pinned key, ignoring the attestation
        if let TrustMode::Recover(key) = self {
            return Ok((*key.clone(), Identity::Recovered(*key.clone())));
        }

        // Attestations verify at the handshake's wall time, in Unix seconds
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_secs();

        // Look the signer up in the roots of every environment. An attestation
        // from a known root that fails to verify is a hard error; only unknown
        // signers fall through to the self-signed check. Keep their diagnostic
        // so an unknown signer is not obscured by the self-signed fallback.
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

        // No trusted root matched. Only an attestation signed by its own
        // identity key can establish a self-signed peer.
        let key =
            trust::device::verify_self_signed(attestation.as_bytes()).map_err(|err| {
                match (err, untrusted) {
                    (trust::Error::NotSelfSigned, Some(err)) => err.to_string(),
                    (err, _) => err.to_string(),
                }
            })?;
        Ok((key.clone(), Identity::SelfSigned(key)))
    }
}

/// Attestation policy and invalid-signer diagnostics.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self_attestation, test_clock};
    use darkbio_crypto::{cbor, cose};

    /// Root trust accepts a self-signed attestation and refuses a foreign
    /// signer, while recovery pins its key whatever the attestation.
    #[test]
    fn test_trust_modes() {
        // Generate the Ark's identity and a foreign signer
        let clock = test_clock().clock();
        let identity = xdsa::SecretKey::generate();
        let foreign = xdsa::SecretKey::generate();

        // Self-signed attestation proves possession of its key only
        let attestation = self_attestation(&identity, identity.public_key(), &clock);
        let (key, info) = TrustMode::RootOrSelf
            .verify(&attestation, clock.system_time())
            .unwrap();
        assert_eq!(key.fingerprint(), identity.public_key().fingerprint());
        assert!(matches!(info, Identity::SelfSigned(_)));
        assert_eq!(info.realm(), None);

        // Attestation by an unknown key, refused
        let attestation = self_attestation(&foreign, identity.public_key(), &clock);
        let error = TrustMode::RootOrSelf
            .verify(&attestation, clock.system_time())
            .err()
            .unwrap();
        assert!(error.contains(&hex::encode(foreign.fingerprint().to_bytes())));
        assert!(error.contains("unknown key"));
        assert!(!error.contains("--features"));

        // Recovery mode, the pinned key is used regardless of the attestation
        let pinned = xdsa::SecretKey::generate().public_key();
        let (key, info) = TrustMode::Recover(Box::new(pinned.clone()))
            .verify(&attestation, clock.system_time())
            .unwrap();
        assert_eq!(key.fingerprint(), pinned.fingerprint());
        assert!(matches!(info, Identity::Recovered(_)));
        assert_eq!(info.realm(), None);
    }

    /// Known device roots verify signatures instead of accepting claimed fingerprints.
    #[test]
    fn test_known_signer() {
        // Sign one attestation with an unrelated key, then relabel its signer
        let clock = test_clock().clock();
        let key = xdsa::SecretKey::generate();
        let attestation = self_attestation(&key, key.public_key(), &clock);
        for fingerprint in [
            "8b842c20bb8083a1635140e58675f3b95a100ac0e39ab82fa6cb2ef23eb532fb", // release hardware
            "7d725c5cb3f80ef4e17bb98ea1f14683714a7eaffaa78cded17ff0e2c0c96ffa", // staging hardware
            "456df8b670cbe2c1c95368f2678ddf66671826542d742cfb5aee2f30e194b2ed", // develop hardware
            "4ae00e993329f6f46e350b247a8e2b38b915ade524ceca70d6530c859d1f69ef", // develop emulator
            "9dfa577f0938f11f9df9a5eadcdc8e57353702dca6d23291cf5cc71a403d67a8", // develop cloud
        ] {
            // Claim the root's fingerprint in the attestation's header
            let bytes: [u8; 32] = hex::decode(fingerprint).unwrap().try_into().unwrap();
            let claimed = xdsa::Fingerprint::from_bytes(&bytes);
            let root = trust::roots::identify(&claimed).unwrap();
            let mut envelope: cose::CoseSign1 = cbor::decode(attestation.as_bytes()).unwrap();
            let mut header: cose::SigProtectedHeader = cbor::decode(&envelope.protected).unwrap();
            header.kid = claimed;
            envelope.protected = cbor::encode(&header).unwrap();
            let forged = Attestation::new(cbor::encode(&envelope).unwrap()).unwrap();

            // A device root fails the signature check, while a cloud root is an
            // untrusted signer the error names
            let error = TrustMode::RootOrSelf
                .verify(&forged, clock.system_time())
                .err()
                .unwrap();
            if root.role == trust::roots::Role::CloudAttester {
                assert!(error.contains(fingerprint), "{error}");
                assert!(error.contains(&root.to_string()), "{error}");
            } else {
                assert!(error.starts_with("cwt:"), "{error}");
            }
            assert!(!error.contains("--features"));
        }
    }

    /// Invalid self-signatures retain their cryptographic error.
    #[test]
    fn test_invalid_attestation() {
        let clock = test_clock().clock();
        let key = xdsa::SecretKey::generate();
        let attestation = self_attestation(&key, key.public_key(), &clock);
        let mut bytes = attestation.as_bytes().to_vec();
        *bytes.last_mut().unwrap() ^= 1;
        let attestation = Attestation::new(bytes).unwrap();
        let error = TrustMode::RootOrSelf
            .verify(&attestation, clock.system_time())
            .err()
            .unwrap();
        assert!(error.starts_with("cwt:"), "{error}");
        assert!(!error.contains("--features"));
        assert!(!error.contains("not among the trusted roots"));
    }
}
