// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! An Ark the host can reach, however it is reached, the hardware plugged in
//! and the emulators running on the machine being one kind of thing to list,
//! tell apart and connect to.

use crate::ark::{Ark, Realm};
use crate::registry::Instance;
use crate::{Error, emulator, usb};
use darkbio_trust::Environment;
use darkbio_wire::transport::Verifier;
use std::fmt;

/// Lists the Arks the host can reach, the hardware plugged in first and the
/// emulators running on the machine after, by port.
pub fn list() -> Result<Vec<Device>, Error> {
    let mut devices = usb::list()?;
    devices.extend(emulator::list()?);
    Ok(devices)
}

/// An Ark the host can reach, before a session with it. What is known of it
/// depends on how it is reached, a facet absent being one the device cannot
/// tell before connecting.
#[derive(Clone)]
pub struct Device {
    kind: Kind, // How the device is reached, with what it said of itself
}

/// The ways a device is reached, each with the record it was found by.
#[derive(Clone)]
enum Kind {
    Usb(nusb::DeviceInfo), // Enumeration record of an Ark plugged in
    Emulator(Instance),    // Listing entry of an emulator running on the host
}

impl Device {
    /// Wraps the enumeration record of an Ark plugged in.
    pub(crate) fn usb(info: nusb::DeviceInfo) -> Self {
        Self {
            kind: Kind::Usb(info),
        }
    }

    /// Wraps the listing entry of an emulator running on the host.
    pub(crate) fn emulator(instance: Instance) -> Self {
        Self {
            kind: Kind::Emulator(instance),
        }
    }

    /// Domain the device belongs to, live for hardware and sandbox for
    /// emulators.
    pub fn realm(&self) -> Realm {
        match self.kind {
            Kind::Usb(_) => Realm::Live,
            Kind::Emulator(_) => Realm::Sandbox,
        }
    }

    /// Whether the device accepts a connection. Hardware plugged in always
    /// does, an emulator once its firmware has booted.
    pub fn ready(&self) -> bool {
        match &self.kind {
            Kind::Usb(_) => true,
            Kind::Emulator(instance) => instance.ready,
        }
    }

    /// Serial the device reports. Hardware always has one, its identity
    /// fingerprint until it is onboarded, an emulator only once onboarded.
    pub fn serial(&self) -> Option<&str> {
        match &self.kind {
            Kind::Usb(info) => info.serial_number(),
            Kind::Emulator(instance) => instance.serial.as_deref(),
        }
    }

    /// Name the device was given, if any.
    pub fn name(&self) -> Option<&str> {
        match &self.kind {
            Kind::Usb(info) => info.product_string().and_then(usb::name),
            Kind::Emulator(instance) => instance.name.as_deref().filter(|name| !name.is_empty()),
        }
    }

    /// File name of the disk image an emulator runs from, what a fresh one is
    /// known by. Hardware has none.
    pub fn image(&self) -> Option<&str> {
        match &self.kind {
            Kind::Usb(_) => None,
            Kind::Emulator(instance) => {
                Some(instance.disk.as_str()).filter(|disk| !disk.is_empty())
            }
        }
    }

    /// Environment the device is bound to, if the build knows of it. An
    /// emulator says once it has booted, hardware only through its
    /// attestation in the handshake.
    pub fn environment(&self) -> Option<Environment> {
        match &self.kind {
            Kind::Usb(_) => None,
            Kind::Emulator(instance) => instance.environment(),
        }
    }

    /// Connects to the device and runs the wire handshake over it, the
    /// verifier deciding whether to trust the attestation it presents. The
    /// handshake has the wire's own budget, the requests of the session wait
    /// `DEFAULT_TIMEOUT` unless changed on it.
    pub fn connect<V: Verifier>(&self, verifier: &V) -> Result<(Ark, V::Info), Error> {
        match &self.kind {
            Kind::Usb(info) => usb::connect(info, verifier),
            Kind::Emulator(instance) => emulator::connect(&instance.url(), verifier),
        }
    }
}

impl fmt::Display for Device {
    /// Labels the device by its name, else its serial, else its image, the
    /// first of them a device always has. A listing entry with none of them
    /// falls back to where the device is reached.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(label) = self
            .name()
            .or_else(|| self.serial())
            .or_else(|| self.image())
        {
            return f.write_str(label);
        }
        match &self.kind {
            Kind::Usb(info) => write!(f, "usb {}:{}", info.bus_id(), info.device_address()),
            Kind::Emulator(instance) => write!(f, "port {}", instance.port),
        }
    }
}

impl fmt::Debug for Device {
    /// Shows the facets of the device, never the record it was found by.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("realm", &self.realm())
            .field("ready", &self.ready())
            .field("serial", &self.serial())
            .field("name", &self.name())
            .field("image", &self.image())
            .field("environment", &self.environment())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Listing entry of an emulator that has only booted far enough to be
    // listed, nothing claimed yet.
    fn instance(port: u16) -> Instance {
        Instance {
            port,
            disk: "ark.img".into(),
            ready: false,
            env: None,
            name: None,
            serial: None,
        }
    }

    // Tests that the facets of an emulator follow what it has claimed so far
    // and that its label falls back from the name through the serial to the
    // image, and to the port with none of them.
    #[test]
    fn test_emulator_facets() {
        let fresh = Device::emulator(instance(18181));
        assert_eq!(fresh.realm(), Realm::Sandbox);
        assert!(!fresh.ready());
        assert_eq!(fresh.serial(), None);
        assert_eq!(fresh.name(), None);
        assert_eq!(fresh.image(), Some("ark.img"));
        assert_eq!(fresh.environment(), None);
        assert_eq!(fresh.to_string(), "ark.img");

        let mut booted = instance(18182);
        booted.ready = true;
        booted.env = Some("staging".into());
        let booted = Device::emulator(booted);
        assert!(booted.ready());
        #[cfg(feature = "staging")]
        assert_eq!(booted.environment(), Some(Environment::Staging));
        #[cfg(not(feature = "staging"))]
        assert_eq!(booted.environment(), None);
        assert_eq!(booted.to_string(), "ark.img");

        let mut onboarded = instance(18183);
        onboarded.serial = Some("abc123".into());
        let onboarded = Device::emulator(onboarded);
        assert_eq!(onboarded.serial(), Some("abc123"));
        assert_eq!(onboarded.to_string(), "abc123");

        let mut named = instance(18184);
        named.serial = Some("abc123".into());
        named.name = Some("lab".into());
        let named = Device::emulator(named);
        assert_eq!(named.to_string(), "lab");

        let mut bare = instance(18185);
        bare.disk = String::new();
        let bare = Device::emulator(bare);
        assert_eq!(bare.image(), None);
        assert_eq!(bare.to_string(), "port 18185");
    }
}
