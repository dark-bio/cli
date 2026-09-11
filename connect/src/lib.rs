// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Discovery, authentication and blocking connections to Ark enclaves.
//!
//! [`list`] discovers hardware and emulators, retaining independent discovery
//! failures. [`hardware::list`] and [`emulator::list`] list either kind alone.
//! Names, serials and launcher metadata are observations; [`Locator`] selects
//! an endpoint without depending on its display label. Authentication happens
//! at [`Device::connect`], using a [`wire::transport::Verifier`] that returns
//! [`Identity`]. Connecting authenticates the Ark without contacting the cloud.
//! [`Device::kind`] records how an Ark was discovered. [`Identity::realm`] comes
//! from a trusted certificate, independently of discovery or the connection.
//!
//! [`Ark`] owns a wire session. Dropping it closes the connection, including
//! pending requests issued through its clonable [`Client`] handles. Wire owns
//! multiplexing, I/O workers, deadlines and incoming queue limits. Connect adds
//! typed request/response pairing, USB/WebSocket adapters and cloud prerequisites.
//!
//! ```no_run
//! use darkbio_connect::{schema::DeviceInfoRequest, TrustMode};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let found = darkbio_connect::list();
//! for error in &found.errors { eprintln!("discovery warning: {error}"); }
//! let (ark, identity) = found.select(None)?.connect(&TrustMode::RootOrSelf)?;
//! let info = ark.client().call_timeout(DeviceInfoRequest {}, Duration::from_secs(2))?;
//! println!("Firmware: {}", info.firmware_version);
//! # Ok(())
//! # }
//! ```
//!
//! [`Client::call`] and [`Client::send`] take an absolute deadline. Reuse it
//! across requests to share an operation's budget. For one request,
//! [`Client::call_timeout`] and [`Client::send_timeout`] take a duration starting
//! at the call. Clients carry no default timeout. Request deadlines cover cloud
//! prerequisites, queueing, sending and accepting a response; discovery, connection
//! setup and response decoding are outside them.
//!
//! ```no_run
//! use darkbio_connect::{Client, Error, schema::{DeviceInfoRequest, PairingStatusRequest}};
//! use std::time::{Duration, Instant};
//!
//! fn inspect(client: &Client) -> Result<(), Error> {
//!     let deadline = Instant::now() + Duration::from_secs(2);
//!     client.call(DeviceInfoRequest {}, deadline)?;
//!     client.call(PairingStatusRequest {}, deadline)?;
//!     Ok(())
//! }
//! ```
//!
//! Requests declare their prerequisites through [`Request::SETUP`]: [`Setup::None`],
//! [`Setup::Cloud`] or [`Setup::Relay`]. Device info, onboarding, pairing status and the sync
//! exchange itself need none. Other requests synchronize cloud keys and time
//! lazily, using the attested environment or [`Device::connect_with_env`]. Client
//! clones share a successful sync for the lifetime of their connection.
//! Concurrent callers join one attempt: the first caller supplies its deadline,
//! and each waiter can expire sooner. Failed attempts can be retried by a later
//! call. Self-signed and recovery connections need an explicit environment for
//! cloud operations; otherwise they return [`Error::MissingEnvironment`].
//! Discovery selects their hardware or emulator registry without establishing
//! trust. An attested realm always takes precedence. The Ark verifies cloud
//! certificates, and the cloud authenticates device proofs using its own registry.
//!
//! [`Client::genuine`] synchronizes if needed, obtains an Ark proof and checks
//! the cloud registry, all under one deadline. It returns a [`Registration`]
//! whose flags explain whether it is active.
//!
//! [`schema::UnlockRequest`] requires [`Setup::Relay`]. Sending it first
//! synchronizes and attaches the companion relay under the supplied deadline.
//! Connect carries encrypted companion traffic internally; unlock needs no
//! application receive loop. Relay attachment is shared by client clones.
//! A later call replaces a failed relay, without replaying the operation that
//! failed. Closing the Ark closes its relay too.
//! Scheduling execution and repairing or deleting slots also require relay setup.
//! Firmware preparation and uploads attach on the Ark's first reverse request,
//! allowing operations that need no companion authorization to proceed without it.
//! Transport ping/pong checks detect dead attachments independently of how long
//! the companion takes to authorize. A refused or expired forwarded request
//! leaves other exchanges on the attachment running.
//!
//! [`Client::firmwares`] lists published firmware for the selected environment,
//! newest first. [`Firmware::is_update_for`] compares against the installed version;
//! the Ark decides whether to accept an update. [`Client::update_firmware`]
//! authorizes, streams, verifies and installs an archive under one deadline,
//! reporting [`UpdateProgress`] on the caller's thread. It checks download length
//! and SHA-256 before device verification.
//! Client clones share an update lock. Success acknowledges installation and a
//! pending reboot; it does not verify the new boot. Failed stages are not retried.
//!
//! [`Client::upload_dataset`] lets the Ark identify a file, obtains approval,
//! streams its bytes and waits for validation and indexing. [`Client::upload_reference`]
//! uses a slot's advertised download, checking its size and SHA-256 before
//! processing. Both report [`UploadProgress`] and share one operation deadline.
//! Two chunks may be outstanding to overlap transfer with device writes. Failed
//! sessions are cancelled when time remains, without replaying the upload.
//! Reference downloads use HTTPS and carry no cloud or package credentials.
//!
//! [`Client::with_package_auth`] supplies a caller-owned authentication callback
//! for private package hosts. It receives the package origin, an optional login
//! redirect and the deadline, returning an optional HTTP header. The configured
//! handle and its clones share the callback. Package requests retry once at the
//! original origin; cloud API and relay requests never receive these credentials.
//! Callers own browser interaction, credential storage and deadline handling.
//!
//! ```no_run
//! use darkbio_connect::{Client, Error, schema::UnlockRequest};
//! use std::time::{Duration, Instant};
//!
//! fn unlock(client: &Client) -> Result<(), Error> {
//!     client.call(UnlockRequest {}, Instant::now() + Duration::from_secs(60))?;
//!     Ok(())
//! }
//! ```
//!
//! [`Ark::recv`] receives requests not claimed by cloud services. Applications
//! using this interface own their handlers. Responders expose wire's completion
//! promises: enqueueing a reply and successfully writing it are separate steps.
//!
//! ```no_run
//! use darkbio_connect::{Ark, Error, Responder, schema::ark_to_host::Content};
//!
//! fn receive(
//!     mut ark: Ark,
//!     mut handle: impl FnMut(Content, Responder) -> Result<(), Error>,
//! ) -> Result<(), Error> {
//!     loop {
//!         let (request, responder) = ark.recv()?;
//!         handle(request, responder)?;
//!     }
//! }
//! ```
//!
//! Create the client and closer before moving Ark to this loop's thread. Close
//! and join it when finished. A handler can check local reply completion with
//! `responder.reply(response, deadline)?.wait()?`; this does not confirm that
//! the peer received or processed it.
//!
//! Handlers can pass an application error implementing [`CodedError`] directly
//! to [`Responder::fail`]. Application codes start at 0x100; named protocol
//! failures use [`schema::Error::reserved`] and [`schema::ReservedErrors`].
//! Wire automatically answers requests outside its schema with `UNKNOWN`.
//! Known requests still reach the handler, which can return `UNSUPPORTED` for
//! operations it never serves or `UNAVAILABLE` when its current state prevents
//! serving them. Dropping a responder without replying produces `UNANSWERED`.
//!
//! [`Client::send`] may wait for cloud prerequisites before queueing the request.
//! It then returns without waiting for output or a response. Wire's output queue
//! is unbounded; callers manage the number of outstanding requests. [`Pending::notify`] lets
//! one channel observe many completions without a waiter thread per request.
//! Timeouts and dropped promises do not cancel operations already received
//! by the device.
//!
//! [`TrustMode::RootOrSelf`] accepts roots enabled by the `release`, `staging`
//! and `develop` crate features, as well as self-signed attestations. Self-signing
//! proves key possession only. Recovery pins a key without checking attestation.
//! Callers requiring stricter trust can supply another verifier returning
//! [`Identity`]. Cloud routing follows the attestation unless explicitly overridden.

pub mod emulator;
pub mod hardware;

mod ark;
mod cloud;
mod dataset;
mod device;
mod discovery;
mod identity;
mod incoming;
mod request;

#[cfg(test)]
mod testing;

pub use ark::{Ark, Client, Closer, Pending};
pub use cloud::{Firmware, Registration, UpdateProgress};
pub use darkbio_trust as trust;
pub use darkbio_wire as wire;
pub use darkbio_wire::protocol::schema;
pub use darkbio_wire::protocol::{CodedError, Promise, Responder};
pub use dataset::UploadProgress;
pub use device::{Device, DeviceKind, Locator};
pub use discovery::{Discovery, list};
pub use identity::{Identity, TrustMode};
pub use request::{Request, Setup};

use darkbio_wire::protocol;
use std::io;

/// Things that can go wrong finding, reaching or talking to an Ark.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Dataset identification, transfer or processing failed.
    #[error("dataset upload failed: {0}")]
    Dataset(String),

    /// A dataset source could not supply its advertised bytes.
    #[error("failed to read dataset: {0}")]
    DatasetRead(io::Error),

    /// Firmware selection, transfer or update state was invalid.
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
