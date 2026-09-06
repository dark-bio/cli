// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use crate::wire::{Identity, TrustMode, WireError};
use darkbio_wire::Client;
use darkbio_wire::protocol::{self, ArkToHost, HostToArk};
use nusb::MaybeFuture;
use nusb::descriptors::TransferType;
use nusb::io::{EndpointRead, EndpointWrite};
use nusb::transfer::{Bulk, Direction, In, Out};
use thiserror::Error;

/// Errors returned by Enclave operations.
#[derive(Error, Debug)]
pub enum EnclaveError {
    #[error("failed to open enclave: {0}")]
    Open(nusb::Error),

    #[error("failed to get enclave configuration: {0}")]
    Configuration(nusb::ActiveConfigurationError),

    #[error("enclave has no bulk transfer interface")]
    NoBulkInterface,

    #[error("failed to claim interface: {0}")]
    ClaimInterface(nusb::Error),

    #[error("failed to open endpoint: {0}")]
    OpenEndpoint(nusb::Error),

    #[error("{0}")]
    Wire(#[from] WireError),

    #[error("enclave error: {msg} (code {code})")]
    Remote { code: u64, msg: String },

    #[error("unexpected response from enclave")]
    UnexpectedResponse,
}

/// Enclave represents a connected Ark over USB.
///
/// Fields are ordered so that drop runs in the right sequence: the wire
/// (which owns the endpoint readers/writers) is dropped first, then the
/// interface claim, then the USB handle.
pub struct Enclave {
    wire: Client<EndpointRead<Bulk>, EndpointWrite<Bulk>>,
    identity: Identity,
    next_id: u64,

    _iface: nusb::Interface,
    _device: nusb::Device,
}

impl Enclave {
    /// Opens an Ark enclave from its USB device info. This claims the first
    /// interface that has bulk IN and OUT endpoints, establishes an encrypted
    /// session, and returns a ready-to-use Enclave.
    pub fn open(info: &nusb::DeviceInfo, trust: &TrustMode) -> Result<Self, EnclaveError> {
        let device = info.open().wait().map_err(EnclaveError::Open)?;

        // Walk the active configuration to locate bulk IN + OUT endpoints.
        let config = device
            .active_configuration()
            .map_err(EnclaveError::Configuration)?;

        let mut bulk_eps = None;
        'search: for iface_group in config.interfaces() {
            for alt in iface_group.alt_settings() {
                let mut found_in = None;
                let mut found_out = None;

                for ep in alt.endpoints() {
                    if ep.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match ep.direction() {
                        Direction::In => found_in = found_in.or(Some(ep.address())),
                        Direction::Out => found_out = found_out.or(Some(ep.address())),
                    }
                }
                if let (Some(in_addr), Some(out_addr)) = (found_in, found_out) {
                    bulk_eps = Some((iface_group.interface_number(), in_addr, out_addr));
                    break 'search;
                }
            }
        }
        let (iface_num, ep_in_addr, ep_out_addr) = bulk_eps.ok_or(EnclaveError::NoBulkInterface)?;

        // Claim the interface and open the endpoints.
        let iface = device
            .claim_interface(iface_num)
            .wait()
            .map_err(EnclaveError::ClaimInterface)?;

        let ep_in = iface
            .endpoint::<Bulk, In>(ep_in_addr)
            .map_err(EnclaveError::OpenEndpoint)?;
        let ep_out = iface
            .endpoint::<Bulk, Out>(ep_out_addr)
            .map_err(EnclaveError::OpenEndpoint)?;

        // Wrap the endpoints into std::io::Read/Write and create the wire.
        let reader = ep_in.reader(64 * 1024);
        let writer = ep_out.writer(64 * 1024);
        let mut wire = Client::new(reader, writer);
        let identity = wire.handshake(trust)?;

        Ok(Self {
            wire,
            identity,
            next_id: 1,
            _iface: iface,
            _device: device,
        })
    }

    /// Returns the identity of the enclave established during session
    /// establishment.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Retrieves the hardware and firmware version info of the enclave.
    pub fn device_info(&mut self) -> Result<protocol::DeviceInfoResponse, EnclaveError> {
        use protocol::host_to_ark::Content;

        let resp = self.request(Content::DeviceInfo(protocol::DeviceInfoRequest {}))?;

        match resp.content {
            Some(protocol::ark_to_host::Content::DeviceInfo(info)) => Ok(info),
            _ => Err(EnclaveError::UnexpectedResponse),
        }
    }

    /// Onboards the enclave with a signed attestation certificate (CWT).
    #[cfg(feature = "internal")]
    pub fn onboard(&mut self, device_attestation: &[u8]) -> Result<(), EnclaveError> {
        use protocol::host_to_ark::Content;

        self.request(Content::Onboard(protocol::OnboardingRequest {
            device_attestation: device_attestation.to_vec(),
        }))?;

        Ok(())
    }

    /// Re-runs the encrypted handshake on the existing connection, refreshing
    /// the cached identity. Used after onboarding so a follow-up status
    /// reflects the freshly injected attestation rather than the pre-onboard
    /// identity captured when the enclave was first opened.
    #[cfg(feature = "internal")]
    pub fn refresh_session(&mut self, trust: &TrustMode) -> Result<(), EnclaveError> {
        self.identity = self.wire.handshake(trust)?;
        self.next_id = 1;
        Ok(())
    }

    /// Sends a request to the enclave and returns the validated response.
    /// If the enclave signals an error in the response envelope, it is
    /// converted into an EnclaveError::Remote.
    fn request(
        &mut self,
        content: protocol::host_to_ark::Content,
    ) -> Result<ArkToHost, EnclaveError> {
        let id = self.next_id;
        self.next_id += 1;

        self.wire.send_message(HostToArk {
            id: Some(id),
            content: Some(content),
        })?;

        let resp = self.wire.next_message()?;
        if let Some(err) = resp.err {
            return Err(EnclaveError::Remote {
                code: err.code,
                msg: err.msg,
            });
        }
        Ok(resp)
    }
}
