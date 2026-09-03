// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Wire between the host and an Ark. The encrypted transport is darkbio-wire,
//! the trust policy deciding which Arks a session is opened with lives here.

pub use darkbio_wire::Error as WireError;

use darkbio_crypto::xdsa;
use darkbio_trust as trust;
use darkbio_trust::Environment;
use darkbio_trust::device::Device;
use darkbio_wire::{Attestation, Verifier};
use std::time::{SystemTime, UNIX_EPOCH};

/// Controls which device attestations are accepted during the handshake.
pub enum TrustMode {
    /// Accept Arks attested under the hardware roots of any environment, or
    /// never onboarded Arks presenting a self-signed attestation.
    RootOrSelf,

    /// Skip the attestation and authenticate the handshake against the given
    /// identity key instead, recovering Arks with a corrupted or missing one.
    Recover(xdsa::PublicKey),
}

/// Identity of the Ark established by the handshake, depending on what the
/// trust mode accepted.
#[derive(Clone)]
pub enum Identity {
    /// Attested under the hardware roots of an environment.
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
            return Ok((key.clone(), Identity::Recovered(key.clone())));
        }
        // Look the signer up in the hardware roots of every environment. An
        // attestation from a known root that fails to verify is a hard error,
        // only unknown signers fall through to the self-signed check.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_secs();

        for environment in [
            Environment::Release,
            Environment::Staging,
            Environment::Develop,
        ] {
            let roots = trust::roots::hardware(environment);
            match trust::device::verify(attestation.as_bytes(), roots, &[], Some(now)) {
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
        // Signed by no known root, accept it if the Ark attested itself
        let key = trust::device::verify_self_signed(attestation.as_bytes())
            .map_err(|err| err.to_string())?;
        Ok((key.clone(), Identity::SelfSigned(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_crypto::cwt;
    use darkbio_crypto::cwt::claims::{self, eat};
    use darkbio_trust::CRYPTO_DOMAIN_DEVICE_ATTESTATION;
    use darkbio_trust::device::HardwareClaims;

    /// Hardware attestation of the identity, signed by the given key.
    fn attest(signer: &xdsa::SecretKey, identity: xdsa::PublicKey) -> Attestation {
        let claims = HardwareClaims {
            sub: claims::Subject {
                sub: "test-device".into(),
            },
            cnf: claims::Confirm::new(identity),
            nbf: claims::NotBefore { nbf: 0 },
            iat: claims::IssuedAt { iat: 0 },
            oem: eat::Oemid::new_pen(0),
            hwm: eat::HwModel { hw_model: vec![] },
            hwv: eat::HwVersion::new("test-version".into()),
        };
        let cwt = cwt::issue(&claims, signer, CRYPTO_DOMAIN_DEVICE_ATTESTATION).unwrap();
        Attestation::new(cwt).unwrap()
    }

    // Tests that the root trust mode accepts a self-signed attestation with the
    // Ark's own identity, refuses one signed by an unknown key, and that the
    // recovery mode pins the given identity regardless of the attestation.
    #[test]
    fn test_trust_modes() {
        let identity = xdsa::SecretKey::generate();
        let foreign = xdsa::SecretKey::generate();

        // Self-signed attestation, accepted as a never onboarded Ark
        let attestation = attest(&identity, identity.public_key());
        let (key, info) = TrustMode::RootOrSelf.verify(&attestation).unwrap();
        assert_eq!(
            key.fingerprint(),
            identity.public_key().fingerprint(),
            "self-signed key mismatch"
        );
        assert!(
            matches!(info, Identity::SelfSigned(_)),
            "self-signed identity mismatch"
        );
        // Attestation by an unknown key, refused
        let attestation = attest(&foreign, identity.public_key());
        assert!(
            TrustMode::RootOrSelf.verify(&attestation).is_err(),
            "foreign attestation accepted"
        );
        // Recovery mode, the pinned key is used regardless of the attestation
        let pinned = xdsa::SecretKey::generate().public_key();
        let (key, info) = TrustMode::Recover(pinned.clone())
            .verify(&attestation)
            .unwrap();
        assert_eq!(
            key.fingerprint(),
            pinned.fingerprint(),
            "pinned key mismatch"
        );
        assert!(
            matches!(info, Identity::Recovered(_)),
            "recovered identity mismatch"
        );
    }
}
