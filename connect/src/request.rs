// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! The pairing of every request body with the body of the response answering
//! it, which the wire leaves to the layer above. The typed calls of the Ark
//! are spelled over it.

use darkbio_wire::protocol::schema::*;
use darkbio_wire::protocol::{self, Message};

/// A request body paired with the body of its response. Implemented for
/// every request of the protocol, and open, so a body the crate does not
/// know yet can be paired by its caller.
pub trait Request: Into<Message> {
    /// Body the Ark answers this request with.
    type Response: TryFrom<Message, Error = protocol::Error>;
}

/// Pairs each request body with its response body.
macro_rules! pairs {
    ($($request:ident => $response:ident,)*) => {
        $(
            impl Request for $request {
                type Response = $response;
            }
        )*
    };
}

pairs! {
    DeviceInfoRequest => DeviceInfoResponse,
    OnboardingRequest => OnboardingResponse,
    GenuinityProofRequest => GenuinityProofResponse,
    UnlockRequest => UnlockResponse,
    CloudSyncStartRequest => CloudSyncStartResponse,
    CloudSyncFinishRequest => CloudSyncFinishResponse,
    FirmwareUpdatePrepRequest => FirmwareUpdatePrepResponse,
    FirmwareUpdateInitRequest => FirmwareUpdateInitResponse,
    FirmwareUpdateUploadRequest => FirmwareUpdateUploadResponse,
    FirmwareUpdateVerifyRequest => FirmwareUpdateVerifyResponse,
    FirmwareUpdateInstallRequest => FirmwareUpdateInstallResponse,
    PairingStatusRequest => PairingStatusResponse,
    PairingAuthRequest => PairingAuthResponse,
    PairingSetAppIdentityRequest => PairingSetAppIdentityResponse,
    PairingSetAppStorageRequest => PairingSetAppStorageResponse,
    PairingAckArkStorageRequest => PairingAckArkStorageResponse,
    PairingAcceptanceRequest => PairingAcceptanceResponse,
    PairingCompletionRequest => PairingCompletionResponse,
    RelayJoinRequest => RelayJoinResponse,
    RelayAppToArkRequest => RelayArkToAppResponse,
    ExecutionUploadStartRequest => ExecutionUploadStartResponse,
    ExecutionUploadChunkRequest => ExecutionUploadChunkResponse,
    ExecutionScheduleRequest => ExecutionScheduleResponse,
    ExecutionStatusRequest => ExecutionStatusResponse,
    ExecutionCancelRequest => ExecutionCancelResponse,
    SlotListRequest => SlotListResponse,
    SlotRepairRequest => SlotRepairResponse,
    SlotDeleteRequest => SlotDeleteResponse,
    SlotUploadPeekRequest => SlotUploadPeekResponse,
    SlotUploadStartRequest => SlotUploadStartResponse,
    SlotUploadChunkRequest => SlotUploadChunkResponse,
    SlotUploadCancelRequest => SlotUploadCancelResponse,
    SlotUploadProcessRequest => SlotUploadProcessResponse,
}
