// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Trust policy deciding which Arks a session is opened with, built on the
//! wire's `Verifier` and the roots of trust of the environments the build
//! was made for, and the identity of the Ark it establishes.

use darkbio_crypto::xdsa;
use darkbio_trust as trust;
use darkbio_trust::Environment;
use darkbio_trust::device::Device;
use darkbio_wire::transport::{Attestation, Verifier};
#[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Environments whose roots the build trusts, each enabled by the crate
/// feature of the same name.
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
    /// environment the build trusts, or never onboarded Arks presenting a
    /// self-signed attestation.
    RootOrSelf,

    /// Skip the attestation and authenticate the handshake against the given
    /// identity key instead, recovering Arks with a corrupted or missing one.
    Recover(Box<xdsa::PublicKey>),
}

/// Identity of the Ark established by the handshake, depending on what the
/// trust mode accepted.
#[derive(Clone)]
pub enum Identity {
    /// Attested under the roots of an environment.
    Attested {
        environment: Environment, // Environment whose root signed the attestation
        device: Device,           // Verified device details from the attestation
    },
    /// Attested by the Ark's own identity key, it was never onboarded.
    SelfSigned(xdsa::PublicKey),
    /// Pinned by the caller, the attestation was not checked.
    Recovered(xdsa::PublicKey),
}

impl Identity {
    /// Identity key of the Ark, the one the handshake was authenticated with.
    pub fn key(&self) -> &xdsa::PublicKey {
        match self {
            Identity::Attested { device, .. } => &device.signer,
            Identity::SelfSigned(key) | Identity::Recovered(key) => key,
        }
    }
}

impl Verifier for TrustMode {
    type Info = Identity;

    fn verify(&self, attestation: &Attestation) -> Result<(xdsa::PublicKey, Identity), String> {
        // Recovery pins the identity, the attestation is not consulted at all
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

            for &environment in ENVIRONMENTS {
                let hardware = trust::roots::hardware(environment);
                let emulator = trust::roots::emulator(environment);
                match trust::device::verify(attestation.as_bytes(), hardware, emulator, Some(now)) {
                    Ok(device) => {
                        return Ok((
                            device.signer.clone(),
                            Identity::Attested {
                                environment,
                                device,
                            },
                        ));
                    }
                    Err(trust::Error::UnexpectedSigner(_)) => continue,
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
        // Signed by no known root, accept it if the Ark attested itself
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

        // Self-signed attestation, accepted as a never onboarded Ark
        let attestation = self_attestation(&identity, identity.public_key());
        let (key, info) = TrustMode::RootOrSelf.verify(&attestation).unwrap();
        assert_eq!(key.fingerprint(), identity.public_key().fingerprint());
        assert!(matches!(info, Identity::SelfSigned(_)));

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
    }
}
