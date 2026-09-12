// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Request/response pairings used by typed client calls.
//! Wire checks message direction and content; this table selects the response type.

use crate::timing::{APPROVAL_WINDOW, PAIRING_WINDOW};
use darkbio_wire::protocol::schema::*;
use darkbio_wire::protocol::{self, Message};
use std::time::Duration;

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

    /// Protocol wait window, including a reply margin, for requests that wait
    /// on a person or device formatting. Replaces an inactivity allowance only;
    /// an absolute caller deadline still applies.
    const WINDOW: Option<Duration> = None;
}

/// Pairs each request body with its response body.
macro_rules! pairs {
    ($setup:expr, $window:expr; $($request:ident => $response:ident,)*) => {
        $(
            impl Request for $request {
                type Response = $response;
                const SETUP: Setup = $setup;
                const WINDOW: Option<Duration> = $window;
            }
        )*
    };
}

pairs! { Setup::None, None;
    DeviceInfoRequest => DeviceInfoResponse,
    OnboardingRequest => OnboardingResponse,
    CloudSyncStartRequest => CloudSyncStartResponse,
    CloudSyncFinishRequest => CloudSyncFinishResponse,
}

pairs! { Setup::Cloud, None;
    GenuinityProofRequest => GenuinityProofResponse,
    FirmwareUpdateInitRequest => FirmwareUpdateInitResponse,
    FirmwareUpdateUploadRequest => FirmwareUpdateUploadResponse,
    FirmwareUpdateVerifyRequest => FirmwareUpdateVerifyResponse,
    FirmwareUpdateInstallRequest => FirmwareUpdateInstallResponse,
    PairingAuthRequest => PairingAuthResponse,
    PairingSetAppIdentityRequest => PairingSetAppIdentityResponse,
    PairingSetAppStorageRequest => PairingSetAppStorageResponse,
    PairingAckArkStorageRequest => PairingAckArkStorageResponse,
    RelayJoinRequest => RelayJoinResponse,
    RelayAppToArkRequest => RelayArkToAppResponse,
    ExecutionUploadStartRequest => ExecutionUploadStartResponse,
    ExecutionUploadChunkRequest => ExecutionUploadChunkResponse,
    ExecutionStatusRequest => ExecutionStatusResponse,
    ExecutionCancelRequest => ExecutionCancelResponse,
    SlotListRequest => SlotListResponse,
    DatasetPathsRequest => DatasetPathsResponse,
    SlotIdentifyRequest => SlotIdentifyResponse,
    SlotUploadChunkRequest => SlotUploadChunkResponse,
    SlotUploadCancelRequest => SlotUploadCancelResponse,
    SlotUploadProcessRequest => SlotUploadProcessResponse,
}

pairs! { Setup::Cloud, Some(APPROVAL_WINDOW);
    FirmwareUpdatePrepRequest => FirmwareUpdatePrepResponse,
    SlotUploadStartRequest => SlotUploadStartResponse,
}

pairs! { Setup::Relay, Some(APPROVAL_WINDOW);
    UnlockRequest => UnlockResponse,
    ExecutionScheduleRequest => ExecutionScheduleResponse,
    SlotRepairRequest => SlotRepairResponse,
    SlotDeleteRequest => SlotDeleteResponse,
}

pairs! { Setup::Cloud, Some(PAIRING_WINDOW);
    PairingAcceptanceRequest => PairingAcceptanceResponse,
    PairingCompletionRequest => PairingCompletionResponse,
}
