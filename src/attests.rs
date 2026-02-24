// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use darkbio_crypto::cbor::Cbor;
use darkbio_crypto::cwt::claims;
use darkbio_crypto::cwt::claims::eat;
use darkbio_crypto::xdsa;

/// Device attestation CWT claims. Used by the wire layer to verify that a
/// connected device is a genuine Dark Bio Ark (root-signed) or a pre-onboarding
/// device (self-signed).
#[derive(Cbor)]
pub struct DeviceAttestation {
    #[cbor(embed)]
    pub sub: claims::Subject,
    #[cbor(embed)]
    pub cnf: claims::Confirm<xdsa::PublicKey>,
    #[cbor(embed)]
    pub nbf: claims::NotBefore,
    #[cbor(embed)]
    pub iat: claims::IssuedAt,
    #[cbor(embed)]
    pub oem: eat::Oemid,
    #[cbor(embed)]
    pub hwm: eat::HwModel,
    #[cbor(embed)]
    pub hwv: eat::HwVersion,
}
