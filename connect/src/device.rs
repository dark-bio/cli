// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Discovered Arks, their reported details and authenticated connections.
//! Discovery metadata is unverified. Connecting establishes the peer's identity.

use crate::emulator::Instance;
use crate::{Ark, Error, Identity, emulator, hardware};
use darkbio_wire::transport::Verifier;
use std::fmt;

/// Kind of Ark reported by discovery. Authentication establishes its identity
/// separately; this classification does not verify the peer's realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// Physical Ark attached to the host.
    Hardware,
    /// Emulated Ark published by a local launcher.
    Emulator,
}

/// Address used to select one discovered Ark, independent of its display name.
///
/// Addresses can change or be reused after an Ark disconnects. A locator is
/// not an authenticated device identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Locator {
    /// Host bus and device address assigned during hardware enumeration.
    Hardware { bus: String, address: u8 },
    /// Host port assigned by the emulator's launcher.
    Emulator { port: u16 },
}

impl fmt::Display for Locator {
    /// Formats the selector accepted by [`crate::Discovery::select`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware { bus, address } => write!(f, "hardware:{bus}:{address}"),
            Self::Emulator { port } => write!(f, "emulator:{port}"),
        }
    }
}

/// Discovered Ark with the details needed to connect to it.
/// Reported metadata remains unverified; connecting returns a separate identity.
#[derive(Clone)]
pub struct Device {
    source: Source, // Discovery record retained for connection and display
}

/// Origin of the discovery record, independent of the Ark's authenticated realm.
#[derive(Clone)]
enum Source {
    Usb(nusb::DeviceInfo),
    Registry(Instance),
}

impl Device {
    /// Retains a USB enumeration record for later connection.
    pub(crate) fn hardware(info: nusb::DeviceInfo) -> Self {
        Self {
            source: Source::Usb(info),
        }
    }

    /// Retains an emulator listing entry for later connection.
    pub(crate) fn emulator(instance: Instance) -> Self {
        Self {
            source: Source::Registry(instance),
        }
    }

    /// Returns the host address of this endpoint, suitable for explicit selection.
    pub fn locator(&self) -> Locator {
        match &self.source {
            Source::Usb(info) => Locator::Hardware {
                bus: info.bus_id().to_owned(),
                address: info.device_address(),
            },
            Source::Registry(instance) => Locator::Emulator {
                port: instance.port,
            },
        }
    }

    /// Returns the kind of Ark reported by discovery, before authentication.
    pub fn kind(&self) -> DeviceKind {
        match self.source {
            Source::Usb(_) => DeviceKind::Hardware,
            Source::Registry(_) => DeviceKind::Emulator,
        }
    }

    /// Returns the readiness last reported by an emulator launcher. Hardware
    /// and launchers omitting readiness return `None`. A report may be stale.
    pub fn ready(&self) -> Option<bool> {
        match &self.source {
            Source::Usb(_) => None,
            Source::Registry(instance) => instance.ready,
        }
    }

    /// Returns the unverified serial from USB enumeration or the launcher.
    /// Empty serials are treated as absent.
    pub fn serial(&self) -> Option<&str> {
        match &self.source {
            Source::Usb(info) => info.serial_number(),
            Source::Registry(instance) => instance.serial.as_deref(),
        }
        .filter(|value| !value.is_empty())
    }

    /// Returns the unverified device name, excluding an empty reported name.
    pub fn name(&self) -> Option<&str> {
        match &self.source {
            Source::Usb(info) => info.product_string().and_then(hardware::name),
            Source::Registry(instance) => instance.name.as_deref(),
        }
        .filter(|value| !value.is_empty())
    }

    /// Returns the emulator image basename, if reported. Several emulators may
    /// use the same basename, so it does not identify an endpoint uniquely.
    pub fn image(&self) -> Option<&str> {
        match &self.source {
            Source::Usb(_) => None,
            Source::Registry(instance) => Some(instance.disk.as_str()),
        }
        .filter(|value| !value.is_empty())
    }

    /// Returns the launcher's unverified environment string, independently of
    /// which trust roots this build enables.
    pub fn env(&self) -> Option<&str> {
        match &self.source {
            Source::Usb(_) => None,
            Source::Registry(instance) => instance.env.as_deref(),
        }
    }

    /// Connects using the retained endpoint details and authenticates the peer
    /// with the supplied verifier. Does not repeat discovery or label selection.
    /// The verifier's identity selects cloud routing for later operations;
    /// connecting itself does not contact the cloud.
    pub fn connect<V: Verifier<Info = Identity>>(
        &self,
        verifier: &V,
    ) -> Result<(Ark, Identity), Error> {
        match &self.source {
            Source::Usb(info) => hardware::connect(info, verifier),
            Source::Registry(instance) => emulator::connect(&instance.url(), verifier),
        }
    }
}

impl fmt::Display for Device {
    /// Shows the first available label, falling back to the endpoint locator.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self
            .name()
            .or_else(|| self.serial())
            .or_else(|| self.image())
        {
            Some(label) => f.write_str(label),
            None => self.locator().fmt(f),
        }
    }
}

impl fmt::Debug for Device {
    /// Shows the locator and reported metadata, without opening the device.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("locator", &self.locator())
            .field("label", &self.to_string())
            .field("ready", &self.ready())
            .field("env", &self.env())
            .finish()
    }
}
