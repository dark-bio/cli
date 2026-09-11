// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Discovery of physical Arks attached to the host.
//! Connect through [`Device::connect`] to authenticate a discovered Ark.

mod usb;

pub(crate) use usb::{connect, name};

use crate::{Device, Error};
use nusb::MaybeFuture;

/// Vendor and product id pairs Arks enumerate with.
const USB_IDS: &[(u16, u16)] = &[
    (0x2e8a, 0x10f1), // Ark I
];

/// Lists physical Arks attached to the host, without opening them.
pub fn list() -> Result<Vec<Device>, Error> {
    let devices = nusb::list_devices().wait().map_err(Error::Usb)?;
    Ok(devices
        .filter(|info| USB_IDS.contains(&(info.vendor_id(), info.product_id())))
        .map(Device::hardware)
        .collect())
}
