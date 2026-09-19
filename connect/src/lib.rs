// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Authenticated connections and protocol workflows for Ark hosts.
//! Internal library target of the CLI package. Its Rust API is unstable and is
//! not a supported integration interface.
//!
//! Discovery lists hardware and local emulators without authenticating them.
//! [`Device::connect`] establishes an encrypted session and returns its
//! [`Identity`], distinguishing root attestation, self-signing and a caller-pinned
//! recovery key. [`Ark`] owns the session; its clonable [`Client`] handles issue
//! requests without keeping it open. Dropping or closing the owner ends the
//! connection and its companion relay.
//!
//! The CLI compiles the release, staging and develop device roots. Self-signed
//! and pinned connections remain available; choosing a cloud route does not
//! change which roots authenticate an attestation.
//!
//! ```no_run
//! use darkbio_connect::{Error, TrustMode, schema};
//! use std::time::{Duration, Instant};
//!
//! # fn main() -> Result<(), Error> {
//! let found = darkbio_connect::list();
//! let device = found.select(None)?;
//! let (ark, identity) = device.connect(&TrustMode::RootOrSelf)?;
//! let info = ark.client().call(
//!     schema::DeviceInfoRequest {}, Instant::now() + Duration::from_secs(10),
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! Requests establish their cloud prerequisites lazily. Cloud synchronization
//! reuses the Ark's reported sync marker and clock when fresh. A refused cloud
//! proof triggers one refresh and authentication retry. Relay attachment follows
//! only when required and is reused while healthy. [`Client::sync`] explicitly
//! refreshes the signed clock and cloud keys; [`Client::attach_relay`] exposes
//! attachment for diagnostics.
//! Status and enrollment work before either step. [`Device::connect_with_env`]
//! selects cloud routing after authentication on the same connection. Self-signed
//! and recovery peers need a caller-selected environment for cloud operations.
//! Routing never changes the handshake's trust result.
//!
//! Calls and workflows accept an [`Instant`](std::time::Instant) for one fixed
//! deadline or [`Timing`] for an inactivity allowance, optionally combined with
//! that deadline. No timeout state lives on the shared client. Approval requests
//! use their protocol window when an inactivity allowance is supplied. Arbitrary
//! readers and progress callbacks run on the caller's thread and must bound their
//! own blocking work.
//!
//! [`Client::identify_dataset`] identifies a file without opening an upload session.
//! [`Client::upload_dataset`] streams a [`Dataset`] and waits for processing.
//! [`Client::update_firmware`] streams a [`Firmware`], obtains cloud access keys,
//! verifies and installs it; success acknowledges installation, not the later
//! reboot. [`Client::execute`] uploads an app, obtains approval and retrieves its
//! result. A failed app returns `success: false`, preserving any output the Ark
//! includes. Failed-app streams require developer output to be enabled in the app.
//! Firmware preparation may require approval, so a rejected proof refreshes cloud
//! keys and returns an error for the caller to retry explicitly.
//!
//! Downloads, package catalogs, version selection, caches, prompts, signal
//! handling and reboot waits belong to callers. Connect accepts readers, checks
//! declared sizes and optional dataset hashes, and does not retry a failed
//! transfer. Progress supplies upload session and execution task IDs for explicit
//! cancellation through another client clone.
//!
//! [`Client::pair`] forwards the existing cloud pairing exchange. Its progress
//! callback supplies the rendezvous for presentation to the owner. Pairing and
//! relay payloads stay opaque; the Ark and companion authenticate their content.
//! Connect interprets only rendezvous routing and relay envelope fields.
//!
//! [`Client::send`] establishes prerequisites and returns a typed [`Pending`]
//! without waiting for the response. Waiting later retains the original deadline;
//! dropping it does not cancel a request. [`Pending::notify`] allows one channel
//! to observe several completions. Callers bound outstanding sends.
//!
//! [`Ark::recv`] receives requests not handled by cloud services. Responders keep
//! wire's completion and automatic reply semantics. Unknown requests receive
//! `UNKNOWN`; dropping a known request's responder produces `UNANSWERED`.
//! Application errors pass through unchanged as [`Error::Remote`].

pub mod emulator;
pub mod hardware;

mod ark;
mod cloud;
mod dataset;
mod device;
mod discovery;
mod execution;
mod identity;
mod incoming;
mod request;
mod timing;

#[cfg(test)]
mod testing;

pub use ark::{Ark, Client, Closer, Pending};
pub use cloud::{CloudAuth, Firmware, PairingProgress, Registration, UpdateProgress, cloud_synced};
pub use darkbio_wire as wire;
pub use darkbio_wire::protocol::schema;
pub use darkbio_wire::protocol::{CodedError, Promise, Responder};
pub use darkbio_wire::trust;
pub use dataset::{Dataset, UploadProgress};
pub use device::{Device, DeviceKind, Locator};
pub use discovery::{Discovery, list};
pub use execution::ExecutionProgress;
pub use identity::{Identity, TrustMode};
pub use request::{Request, Setup};
pub use timing::Timing;

/// Version of the CLI package containing this connection library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use darkbio_wire::protocol;
use std::io;

/// Things that can go wrong finding, reaching or talking to an Ark.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The cloud pairing rendezvous or an opaque exchange failed.
    #[error("pairing failed: {0}")]
    Pairing(String),

    /// The companion did not join before the cloud rendezvous expired.
    #[error("pairing timed out")]
    PairingExpired,

    /// App transfer or execution status was invalid.
    #[error("execution failed: {0}")]
    Execution(String),

    /// An app source could not supply its advertised bytes.
    #[error("failed to read app: {0}")]
    ExecutionRead(io::Error),

    /// Dataset identification, transfer or processing failed.
    #[error("dataset upload failed: {0}")]
    Dataset(String),

    /// A source did not match its declared size or digest.
    #[error("source verification failed: {0}")]
    Integrity(String),

    /// The cloud refused the device proof; identity and timestamp failures share
    /// this response deliberately.
    #[error("the cloud rejected the device proof")]
    ProofRejected,

    /// A dataset source could not supply its advertised bytes.
    #[error("failed to read dataset: {0}")]
    DatasetRead(io::Error),

    /// A firmware source could not supply its advertised bytes.
    #[error("failed to read firmware: {0}")]
    FirmwareRead(io::Error),

    /// Firmware transfer or update state was invalid.
    #[error("firmware update failed: {0}")]
    Firmware(String),

    /// A relay connection or forwarded exchange failed.
    #[error("relay operation failed: {0}")]
    Relay(String),

    /// A connection worker could not be started.
    #[error("failed to start connection worker: {0}")]
    Worker(io::Error),
    /// Neither the attestation nor the caller selected a cloud environment.
    #[error("cloud environment unknown; specify an environment when connecting")]
    MissingEnvironment,

    /// A cloud request failed or its response could not be used.
    #[error("cloud operation failed: {0}")]
    Cloud(String),

    /// A protected cloud host needs caller authentication, independently of Ark trust.
    #[error("{message}")]
    CloudAuth {
        /// HTTPS origin to authenticate with the caller's login helper.
        origin: String,
        /// Safe diagnostic without credentials or the rejected request's proof.
        message: String,
    },

    /// Discovery returned no devices and no selector was supplied.
    #[error("no Ark enclave found")]
    NotFound,

    /// No endpoint matches the supplied locator or label.
    #[error("no Ark enclave matches {0}")]
    NoMatch(String),

    /// Several endpoints match; their locators let the caller distinguish them.
    #[error("multiple Ark enclaves match; select an endpoint by its locator")]
    Ambiguous(Vec<Locator>),

    /// Enumerating, opening or configuring a USB device failed.
    #[error("USB operation failed: {0}")]
    Usb(nusb::Error),

    /// The device is held by another program, a browser tab included.
    #[error("device in use by another program: {0}")]
    Busy(nusb::Error),

    /// The device has no vendor interface with bulk endpoints, so it is not
    /// an Ark the wire can be spoken over.
    #[error("device has no vendor interface with bulk endpoints")]
    Unsupported,

    /// The WebSocket endpoint could not be reached.
    #[error("failed to reach WebSocket endpoint: {0}")]
    Unreachable(io::Error),

    /// The endpoint refused the WebSocket upgrade.
    #[error("failed to open websocket: {0}")]
    Upgrade(tungstenite::Error),

    /// The listing of the emulators running on the host could not be read.
    #[error("failed to list emulators: {0}")]
    Registry(io::Error),

    /// The wire handshake failed, the attestation refused by the verifier or
    /// the exchange itself broken.
    #[error("handshake failed: {0}")]
    Handshake(protocol::Error),

    /// A device or cloud request ran past its deadline, or the handshake past
    /// the wire's budget.
    #[error("operation timed out")]
    Timeout,

    /// The Ark refused this request with an application or reserved protocol
    /// error. These replies do not by themselves end the connection.
    #[error("ark error: {} (code {})", .0.msg, .0.code)]
    Remote(schema::Error),

    /// The wire refused a request or its answer, too large, sent in the
    /// wrong direction or answered with an unexpected body.
    #[error("{0}")]
    Protocol(protocol::Error),

    /// The session ended without a close, the reason inside.
    #[error("ark disconnected: {0}")]
    Disconnected(protocol::Error),

    /// The session was closed locally.
    #[error("ark closed")]
    Closed,
}

impl From<protocol::Error> for Error {
    /// Maps a failure of the wire's protocol layer to the connection's error.
    /// The failures ending the session surface as a disconnect, so a request
    /// refused after the Ark went away names the reason.
    fn from(err: protocol::Error) -> Self {
        match err {
            protocol::Error::Timeout => Error::Timeout,
            protocol::Error::Closed => Error::Closed,
            protocol::Error::Remote(err) => Error::Remote(err),
            protocol::Error::UnexpectedResponse { .. }
            | protocol::Error::WrongDirection(_)
            | protocol::Error::TooLarge(_) => Error::Protocol(err),
            err => Error::Disconnected(err),
        }
    }
}
