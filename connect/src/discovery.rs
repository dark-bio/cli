// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Hardware and emulator discovery, with selection by locator or reported label.

use crate::{Device, DeviceKind, Error, emulator, hardware};

/// Devices found and errors reported by independent discovery sources.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Endpoints returned by sources whose enumeration succeeded.
    pub devices: Vec<Device>,
    /// Enumeration failures, retained even when another source found devices.
    pub errors: Vec<Error>,
}

impl Discovery {
    /// Adds one source's result without discarding earlier discoveries or errors.
    fn extend(&mut self, result: Result<Vec<Device>, Error>) {
        match result {
            Ok(devices) => self.devices.extend(devices),
            Err(error) => self.errors.push(error),
        }
    }

    /// Selects an endpoint by locator or by a unique serial, name or image basename.
    /// Without a selector, requires exactly one device. The `hardware:` and
    /// `emulator:` prefixes are reserved for locators; a missing locator never
    /// falls back to a device name. Display formatting does not determine selection.
    pub fn select(&self, selector: Option<&str>) -> Result<&Device, Error> {
        // A reported name must not shadow a locator, including an absent one.
        if let Some(selector) = selector
            && (selector.starts_with("hardware:") || selector.starts_with("emulator:"))
        {
            return self
                .devices
                .iter()
                .find(|device| device.locator().to_string() == selector)
                .ok_or_else(|| Error::NoMatch(selector.into()));
        }
        // Descriptive labels may be shared. Retain every match for ambiguity errors.
        let matches: Vec<_> = self
            .devices
            .iter()
            .filter(|device| {
                selector.is_none_or(|selector| {
                    match selector {
                        "hardware" => return device.kind() == DeviceKind::Hardware,
                        "emulator" => return device.kind() == DeviceKind::Emulator,
                        _ => {}
                    }
                    [device.serial(), device.name(), device.image()]
                        .into_iter()
                        .flatten()
                        .any(|value| value == selector)
                })
            })
            .collect();
        match matches.as_slice() {
            [] => Err(selector.map_or(Error::NotFound, |selector| Error::NoMatch(selector.into()))),
            [device] => Ok(device),
            _ => Err(Error::Ambiguous(
                matches.into_iter().map(Device::locator).collect(),
            )),
        }
    }
}

/// Lists hardware and emulators. A discovery failure for one kind does not hide
/// devices returned by the other.
pub fn list() -> Discovery {
    let mut found = Discovery::default();
    found.extend(hardware::list());
    found.extend(emulator::list());
    found
}

/// Endpoint selection and partial discovery regressions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKind, Locator, emulator::Instance};

    /// Creates an emulator entry with a shared image basename and an optional name.
    fn device(port: u16, name: Option<&str>) -> Device {
        Device::emulator(Instance {
            port,
            disk: "ark.img".into(),
            ready: Some(false),
            env: Some("develop".into()),
            name: name.map(str::to_owned),
            serial: None,
        })
    }

    /// Duplicate labels require explicit locators. Reported names cannot shadow
    /// locators, including an endpoint that has disappeared.
    #[test]
    fn test_selection() {
        let mut found = Discovery::default();
        found.extend(Ok(vec![device(18181, None), device(18182, None)]));
        assert_eq!(found.devices[0].to_string(), found.devices[1].to_string());
        assert!(matches!(
            found.select(Some("ark.img")),
            Err(Error::Ambiguous(_))
        ));
        assert_eq!(
            found.select(Some("emulator:18182")).unwrap().locator(),
            Locator::Emulator { port: 18182 }
        );
        found.devices[0] = device(18181, Some("emulator:18182"));
        assert_eq!(
            found.select(Some("emulator:18182")).unwrap().locator(),
            Locator::Emulator { port: 18182 }
        );
        found.devices.pop();
        assert!(matches!(
            found.select(Some("emulator:18182")),
            Err(Error::NoMatch(_))
        ));
        found.devices[0] = device(18181, Some("hardware:1:2"));
        assert!(matches!(
            found.select(Some("hardware:1:2")),
            Err(Error::NoMatch(_))
        ));
    }

    /// One source's failure preserves other endpoints and their reported metadata.
    #[test]
    fn test_partial_discovery() {
        let mut found = Discovery::default();
        found.extend(Err(Error::Registry(std::io::Error::other("unavailable"))));
        found.extend(Ok(vec![device(18181, None)]));
        assert_eq!(found.errors.len(), 1);
        let selected = found.select(None).unwrap();
        assert_eq!(selected.env(), Some("develop"));
        assert_eq!(selected.ready(), Some(false));
        assert_eq!(selected.kind(), DeviceKind::Emulator);
    }
}
