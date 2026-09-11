// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Request/response pairings used by typed client calls.
//! Wire checks message direction and content; this table selects the response type.

use darkbio_wire::protocol::schema::*;
use darkbio_wire::protocol::{self, Message};

/// Prerequisites established before a request is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// No cloud services required.
    None,
    /// Cloud keys and a synchronized clock.
    Cloud,
    /// Cloud synchronization followed by companion relay attachment.
    Relay,
}

/// Request body with the response type selected by [`crate::Client::call`].
/// Implemented for the public request bodies. Callers may also pair their own
/// wrappers when those wrappers convert into wire's [`Message`].
pub trait Request: Into<Message> {
    /// Body the Ark answers this request with.
    type Response: TryFrom<Message, Error = protocol::Error>;

    /// Setup required before sending. Wrappers default to cloud synchronization.
    const SETUP: Setup = Setup::Cloud;
}

/// Pairs each request body with its response body.
macro_rules! pairs {
    ($setup:expr; $($request:ident => $response:ident,)*) => {
        $(
            impl Request for $request {
                type Response = $response;
                const SETUP: Setup = $setup;
            }
        )*
    };
}

pairs! { Setup::None;
    DeviceInfoRequest => DeviceInfoResponse,
    OnboardingRequest => OnboardingResponse,
    CloudSyncStartRequest => CloudSyncStartResponse,
    CloudSyncFinishRequest => CloudSyncFinishResponse,
    PairingStatusRequest => PairingStatusResponse,
}

pairs! { Setup::Cloud;
    GenuinityProofRequest => GenuinityProofResponse,
    FirmwareUpdatePrepRequest => FirmwareUpdatePrepResponse,
    FirmwareUpdateInitRequest => FirmwareUpdateInitResponse,
    FirmwareUpdateUploadRequest => FirmwareUpdateUploadResponse,
    FirmwareUpdateVerifyRequest => FirmwareUpdateVerifyResponse,
    FirmwareUpdateInstallRequest => FirmwareUpdateInstallResponse,
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
    ExecutionStatusRequest => ExecutionStatusResponse,
    ExecutionCancelRequest => ExecutionCancelResponse,
    SlotListRequest => SlotListResponse,
    SlotUploadPeekRequest => SlotUploadPeekResponse,
    SlotUploadStartRequest => SlotUploadStartResponse,
    SlotUploadChunkRequest => SlotUploadChunkResponse,
    SlotUploadCancelRequest => SlotUploadCancelResponse,
    SlotUploadProcessRequest => SlotUploadProcessResponse,
}

pairs! { Setup::Relay;
    UnlockRequest => UnlockResponse,
    ExecutionScheduleRequest => ExecutionScheduleResponse,
    SlotRepairRequest => SlotRepairResponse,
    SlotDeleteRequest => SlotDeleteResponse,
}
