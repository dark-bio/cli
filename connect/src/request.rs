// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Request/response pairings used by typed client calls.
//! Wire checks message direction and content; this table selects the response type.

use darkbio_wire::protocol::schema::*;
use darkbio_wire::protocol::{self, Message};

/// Request body with the response type selected by [`crate::Client::call`].
/// Implemented for the public request bodies. Callers may also pair their own
/// wrappers when those wrappers convert into wire's [`Message`].
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
