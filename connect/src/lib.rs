// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Discovery, authentication and blocking connections to Ark enclaves.
//!
//! [`list`] discovers hardware and emulators, retaining independent discovery
//! failures. [`hardware::list`] and [`emulator::list`] list either kind alone.
//! Names, serials and launcher metadata are observations; [`Locator`] selects
//! an endpoint without depending on its display label. Authentication happens
//! at [`Device::connect`], using the caller's [`wire::transport::Verifier`].
//! [`Device::kind`] records how an Ark was discovered. [`Identity::realm`] comes
//! from a trusted certificate, independently of discovery or the connection.
//!
//! [`Ark`] owns a wire session. Dropping it closes the connection, including
//! pending requests issued through its clonable [`Client`] handles. Wire owns
//! multiplexing, I/O workers, deadlines and incoming queue limits. Connect adds
//! typed request/response pairing and USB/WebSocket adapters.
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
//! at the call. Clients carry no default timeout. Request deadlines cover queueing,
//! sending and accepting a response; discovery, connection setup and response
//! decoding are outside them.
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
//! Operations such as unlock need a companion response before they can finish.
//! Keep [`Ark::recv`] running while clients wait for those operations. The
//! application decides how to dispatch incoming requests, forward opaque relay
//! messages and manage its handlers. Responders expose wire's completion
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
//! [`Client::send`] queues immediately; wire's output queue is unbounded.
//! Callers manage the number of outstanding requests. [`Pending::notify`] lets
//! one channel observe many completions without a waiter thread per request.
//! Timeouts and dropped promises do not cancel operations already received
//! by the device.
//!
//! [`TrustMode::RootOrSelf`] accepts roots enabled by the `release`, `staging`
//! and `develop` crate features, as well as self-signed attestations. Self-signing
//! proves key possession only. Recovery pins a key without checking attestation.
//! Callers requiring stricter trust can supply another verifier.

pub mod emulator;
pub mod hardware;

mod ark;
mod device;
mod discovery;
mod identity;
mod request;

#[cfg(test)]
mod testing;

pub use ark::{Ark, Client, Pending};
pub use darkbio_trust as trust;
pub use darkbio_wire as wire;
pub use darkbio_wire::protocol::schema;
pub use darkbio_wire::protocol::{Closer, CodedError, Promise, Responder};
pub use device::{Device, DeviceKind, Locator};
pub use discovery::{Discovery, list};
pub use identity::{Identity, TrustMode};
pub use request::Request;

use darkbio_wire::protocol;
use std::io;

/// Things that can go wrong finding, reaching or talking to an Ark.
#[derive(Debug, thiserror::Error)]
pub enum Error {
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

    /// A request ran past its deadline, or the handshake past the wire's
    /// budget.
    #[error("ark timed out")]
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
