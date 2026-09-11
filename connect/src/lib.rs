// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Connections to Arks from a host process, the layer above the wire that
//! finds a device, reaches it and keeps a session with it for the caller to
//! use:
//!
//!   - Discovery: the Arks plugged into the host and the emulators running
//!     on it, one kind of device to list, tell apart and connect to.
//!   - Transports: genuine Arks are reached over their USB bulk endpoints,
//!     emulators over the WebSocket their firmware serves the same byte
//!     stream on. Both are plain readers and writers to the wire, and any
//!     other stream attaches the same way.
//!   - Sessions: the wire handshake under the wire's own budget, the session
//!     ending reported with its reason, a USB unplug, the emulator shutting
//!     down or the Ark dropping it, and a close tearing everything down.
//!   - Requests: the protobuf protocol as typed calls on an Ark, each answered
//!     or timed out on its own, issued from any number of threads, and sent
//!     ahead of their answers when a caller wants to pipeline. The requests
//!     the Ark sends on its own are handed to a handler with the responder to
//!     answer through.
//!
//! Trust stays the caller's decision through the wire's `Verifier`. The
//! `TrustMode` here accepts the Arks attested under the roots of every
//! environment the build was made for, or never onboarded ones attesting
//! themselves, and pins a known identity for recovery, anything else being
//! the wire's `Roots` or a pinned key. Which environments a build trusts is
//! decided by the crate features of the same names.
//!
//! The crate mirrors the connect package of the dashboard, the same
//! connections from a process instead of a page.

pub mod emulator;
pub mod usb;

mod ark;
mod device;
mod identity;
mod link;
mod registry;
mod request;

#[cfg(test)]
mod testing;

pub use ark::{Ark, DEFAULT_TIMEOUT, Pending, Realm, Responder};
pub use darkbio_trust as trust;
pub use darkbio_wire as wire;
pub use darkbio_wire::protocol::schema;
pub use device::{Device, list};
pub use identity::{Identity, TrustMode};
pub use request::Request;

use darkbio_wire::protocol;
use std::io;

/// Things that can go wrong reaching or talking to an Ark.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Enumerating, opening or configuring a USB device failed.
    #[error("failed to open usb device: {0}")]
    Usb(nusb::Error),

    /// The device is held by another program, a browser tab included.
    #[error("device in use by another program: {0}")]
    Busy(nusb::Error),

    /// The device has no vendor interface with bulk endpoints, so it is not
    /// an Ark the wire can be spoken over.
    #[error("device has no vendor interface with bulk endpoints")]
    Unsupported,

    /// The emulator's socket could not be reached.
    #[error("failed to reach emulator: {0}")]
    Unreachable(io::Error),

    /// The emulator refused the WebSocket upgrade.
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

    /// The Ark answered the request with an error of its own.
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
