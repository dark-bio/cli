// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use crate::wire::{SessionInfo, TrustMode, Wire, WireError};
use crate::wire_protocol::{self, ArkToHost, HostToArk};

use nusb::descriptors::TransferType;
use nusb::io::{EndpointRead, EndpointWrite};
use nusb::transfer::{Bulk, Direction, In, Out};
use nusb::MaybeFuture;
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
    wire: Wire<EndpointRead<Bulk>, EndpointWrite<Bulk>>,
    session_info: SessionInfo,
    next_id: u64,

    _iface: nusb::Interface,
    _device: nusb::Device,
}

impl Enclave {
    /// Opens an Ark enclave from its USB device info. This claims the first
    /// interface that has bulk IN and OUT endpoints, establishes an encrypted
    /// session, and returns a ready-to-use Enclave.
    pub fn open(info: &nusb::DeviceInfo, trust: TrustMode) -> Result<Self, EnclaveError> {
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
        let mut wire = Wire::new(reader, writer);
        let session_info = wire.establish_session(trust)?;

        Ok(Self {
            wire,
            session_info,
            next_id: 1,
            _iface: iface,
            _device: device,
        })
    }

    /// Returns device information extracted from the CWT during session
    /// establishment.
    pub fn session_info(&self) -> &SessionInfo {
        &self.session_info
    }

    /// Performs a handshake to retrieve enclave identity and version info.
    pub fn handshake(&mut self) -> Result<wire_protocol::HandshakeResponse, EnclaveError> {
        use wire_protocol::host_to_ark::Content;

        let resp = self.request(Content::Handshake(wire_protocol::HandshakeRequest {}))?;

        match resp.content {
            Some(wire_protocol::ark_to_host::Content::Handshake(hs)) => Ok(hs),
            _ => Err(EnclaveError::UnexpectedResponse),
        }
    }

    /// Onboards the enclave with a signed attestation certificate (CWT).
    #[cfg(feature = "internal")]
    pub fn onboard(&mut self, device_attestation: &[u8]) -> Result<(), EnclaveError> {
        use wire_protocol::host_to_ark::Content;

        self.request(Content::Onboard(wire_protocol::OnboardingRequest {
            device_attestation: device_attestation.to_vec(),
        }))?;

        Ok(())
    }

    /// Sends a request to the enclave and returns the validated response.
    /// If the enclave signals an error in the response envelope, it is
    /// converted into an EnclaveError::Remote.
    fn request(
        &mut self,
        content: wire_protocol::host_to_ark::Content,
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
