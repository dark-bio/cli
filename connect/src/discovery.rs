// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Hardware and emulator discovery, with selection by locator or reported label.

use crate::{Device, Error, Locator, emulator, hardware};

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

    /// Selects an endpoint by locator or by a unique serial, name or image
    /// basename.
    ///
    /// Without a selector, it requires exactly one device. The `hardware:` and
    /// `emulator:` prefixes are reserved for locators; a missing locator never
    /// falls back to a device name. Display formatting does not determine
    /// selection. The bare names `hardware` and `emulator` require one device of
    /// that kind. Label matches are exact and case-sensitive.
    pub fn select(&self, selector: Option<&str>) -> Result<&Device, Error> {
        let entries = self.devices.iter().map(|device| {
            (
                device.locator(),
                [device.serial(), device.name(), device.image()],
            )
        });
        select(entries, selector).map(|index| &self.devices[index])
    }
}

/// Returns the index of the one endpoint `selector` names, by locator, kind
/// or label.
fn select<'a>(
    entries: impl Iterator<Item = (Locator, [Option<&'a str>; 3])>,
    selector: Option<&str>,
) -> Result<usize, Error> {
    // Keep every locator so an ambiguity hint can distinguish repeated kinds
    let mut available = Vec::new();
    let mut matches = Vec::new();
    for (locator, labels) in entries {
        let matched = selector.is_none_or(|selector| match selector {
            "hardware" => matches!(locator, Locator::Hardware { .. }),
            "emulator" => matches!(locator, Locator::Emulator { .. }),
            // Reported labels cannot shadow locators, including absent ones
            _ if selector.starts_with("hardware:") || selector.starts_with("emulator:") => {
                locator.to_string() == selector
            }
            _ => labels.into_iter().flatten().any(|label| label == selector),
        });
        if matched {
            matches.push(available.len());
        }
        available.push(locator);
    }

    // Select a unique match, and report every match otherwise
    match matches.as_slice() {
        [] => Err(selector.map_or(Error::NotFound, |selector| Error::NoMatch(selector.into()))),
        [index] => Ok(*index),
        _ => Err(Error::Ambiguous {
            matches: matches
                .into_iter()
                .map(|index| available[index].clone())
                .collect(),
            available,
        }),
    }
}

/// Lists hardware and emulators.
///
/// A discovery failure for one kind does not hide devices returned by the other.
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

    /// Duplicate labels require explicit locators, and reported names never
    /// shadow a locator, even of an endpoint that has disappeared.
    #[test]
    fn test_selection() {
        // Two emulators sharing an image basename need their locators
        let mut found = Discovery::default();
        found.extend(Ok(vec![device(18181, None), device(18182, None)]));
        assert_eq!(found.devices[0].to_string(), found.devices[1].to_string());
        assert!(matches!(
            found.select(Some("ark.img")),
            Err(Error::Ambiguous { .. })
        ));
        assert_eq!(
            found.select(Some("emulator:18182")).unwrap().locator(),
            Locator::Emulator { port: 18182 }
        );

        // A name spelling another endpoint's locator does not shadow it
        found.devices[0] = device(18181, Some("emulator:18182"));
        assert_eq!(
            found.select(Some("emulator:18182")).unwrap().locator(),
            Locator::Emulator { port: 18182 }
        );

        // A locator with no endpoint behind it matches no reported name, for
        // emulators and hardware alike
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

    /// A shared name's ambiguity keeps every discovered Ark, matched or not.
    #[test]
    fn test_shared_name_keeps_all_discovered_kinds() {
        let hardware = Locator::Hardware {
            bus: "00".into(),
            address: 1,
        };
        let other = Locator::Hardware {
            bus: "00".into(),
            address: 2,
        };
        let emulator = Locator::Emulator { port: 18181 };
        let entries = [
            (hardware.clone(), [None, Some("Shared Ark"), None]),
            (other.clone(), [None, Some("Other Ark"), None]),
            (
                emulator.clone(),
                [None, Some("Shared Ark"), Some("ark.img")],
            ),
        ];
        let Err(Error::Ambiguous { matches, available }) =
            select(entries.clone().into_iter(), Some("Shared Ark"))
        else {
            panic!("shared name must be ambiguous");
        };
        assert_eq!(matches, [hardware.clone(), emulator.clone()]);
        assert_eq!(available, [hardware, other, emulator]);
        assert!(matches!(
            select(entries.clone().into_iter(), Some("hardware")),
            Err(Error::Ambiguous { .. })
        ));
        assert_eq!(
            select(entries.clone().into_iter(), Some("hardware:00:1")).unwrap(),
            0
        );
        assert_eq!(select(entries.into_iter(), Some("emulator")).unwrap(), 2);
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
