// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Device authentication through wire's verifier and the enabled roots of trust.
//! The returned identity records whether the peer was attested, self-signed or pinned.

use darkbio_crypto::xdsa;
use darkbio_trust as trust;
use darkbio_trust::Environment;
use darkbio_trust::device::Device;
use darkbio_wire::transport::{Attestation, Verifier};
#[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Trusted environments, each enabled by the crate feature of the same name.
#[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
const ENVIRONMENTS: &[Environment] = &[
    #[cfg(feature = "release")]
    Environment::Release,
    #[cfg(feature = "staging")]
    Environment::Staging,
    #[cfg(feature = "develop")]
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
            Identity::Attested { device, .. } => &device.signer,
            Identity::SelfSigned(key) | Identity::Recovered(key) => key,
        }
    }
}

impl Verifier for TrustMode {
    type Info = Identity;

    /// Verifies the attestation or returns the pinned key selected for recovery.
    fn verify(&self, attestation: &Attestation) -> Result<(xdsa::PublicKey, Identity), String> {
        // Recovery authenticates key possession without consulting the attestation.
        if let TrustMode::Recover(key) = self {
            return Ok((*key.clone(), Identity::Recovered(*key.clone())));
        }
        // Look the signer up in the roots of every environment. An attestation
        // from a known root that fails to verify is a hard error, only unknown
        // signers fall through to the self-signed check. A build without any
        // environment has no roots to look up.
        #[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|err| err.to_string())?
                .as_secs();

            for &env in ENVIRONMENTS {
                let hardware = trust::roots::hardware(env);
                let emulator = trust::roots::emulator(env);
                match trust::device::verify(attestation.as_bytes(), hardware, emulator, Some(now)) {
                    Ok(device) => {
                        return Ok((device.signer.clone(), Identity::Attested { env, device }));
                    }
                    Err(trust::Error::UnexpectedSigner(_)) => continue,
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
        // No trusted root matched. Only an attestation signed by its own identity
        // key can establish a self-signed peer.
        let key = trust::device::verify_self_signed(attestation.as_bytes())
            .map_err(|err| err.to_string())?;
        Ok((key.clone(), Identity::SelfSigned(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::self_attestation;

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
        assert!(TrustMode::RootOrSelf.verify(&attestation).is_err());

        // Recovery mode, the pinned key is used regardless of the attestation
        let pinned = xdsa::SecretKey::generate().public_key();
        let (key, info) = TrustMode::Recover(Box::new(pinned.clone()))
            .verify(&attestation)
            .unwrap();
        assert_eq!(key.fingerprint(), pinned.fingerprint());
        assert!(matches!(info, Identity::Recovered(_)));
        assert_eq!(info.realm(), None);
    }
}
