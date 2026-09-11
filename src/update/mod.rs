// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Firmware selection and terminal progress for the update command.

#[cfg(any(feature = "develop", feature = "staging"))]
mod access;

use crate::{Error, connect, find_enclave};
use console::style;
use darkbio_connect::trust::Environment;
use darkbio_connect::{Firmware, TrustMode, UpdateProgress, schema};
use std::time::{Duration, Instant};

pub(super) fn run(
    selector: Option<&str>,
    env: Option<Environment>,
    version: Option<&str>,
    check: bool,
    timeout: u64,
) -> Result<(), Error> {
    let endpoint = find_enclave(selector)?;
    let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
    let client = ark.client();
    #[cfg(any(feature = "develop", feature = "staging"))]
    let client = client.with_package_auth(access::authenticate());
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(timeout))
        .ok_or_else(|| "update timeout is too large".to_owned())?;
    let info = client.call(schema::DeviceInfoRequest {}, deadline)?;
    let firmwares = client.firmwares(deadline)?;
    let Some(firmware) = select(&firmwares, version, &info.firmware_version)? else {
        println!(
            "No newer firmware is available (installed {}).",
            style(&info.firmware_version).bold()
        );
        return Ok(());
    };
    println!(
        "Firmware {} → {} ({:.1} MiB)",
        style(&info.firmware_version).dim(),
        style(&firmware.version).bold(),
        firmware.size as f64 / (1024.0 * 1024.0)
    );
    if !firmware.summary.is_empty() {
        println!("{}", style(&firmware.summary).dim());
    }
    if check {
        return Ok(());
    }

    let pairing = client.call(schema::PairingStatusRequest {}, deadline)?;
    if pairing.paired {
        eprintln!(
            "{}",
            style(if pairing.opened {
                "Approve the firmware update in your companion app."
            } else {
                "Press the Ark's pairing button when it requests update approval."
            })
            .dim()
        );
    }
    let mut previous = None;
    client.update_firmware(firmware, deadline, |progress| match progress {
        UpdateProgress::Preparing => eprintln!("{}", style("Preparing firmware update…").dim()),
        UpdateProgress::Uploading { uploaded, total } => {
            let percent = (u128::from(uploaded) * 100 / u128::from(total)) as u64;
            if previous != Some(percent / 10) || uploaded == total {
                eprintln!(
                    "{}",
                    style(format!(
                        "Uploading: {percent}% ({:.1}/{:.1} MiB)",
                        uploaded as f64 / (1024.0 * 1024.0),
                        total as f64 / (1024.0 * 1024.0)
                    ))
                    .dim()
                );
                previous = Some(percent / 10);
            }
        }
        UpdateProgress::Verifying => eprintln!("{}", style("Verifying firmware on the Ark…").dim()),
        UpdateProgress::Installing => eprintln!("{}", style("Installing firmware…").dim()),
    })?;
    println!(
        "{}",
        style(format!(
            "Firmware {} installed. The Ark is rebooting.",
            firmware.version
        ))
        .green()
    );
    Ok(())
}

/// The package list is newest first. Explicit targets are passed to the Ark,
/// which decides whether the requested update is allowed.
fn select<'a>(
    firmwares: &'a [Firmware],
    requested: Option<&str>,
    installed: &str,
) -> Result<Option<&'a Firmware>, Error> {
    let firmware = match requested {
        Some(version) => firmwares
            .iter()
            .find(|firmware| firmware.version == version)
            .ok_or_else(|| {
                format!("firmware {version} is not published for this Ark's environment")
            })?,
        None => firmwares.first().ok_or_else(|| {
            "no firmware has been published for this Ark's environment".to_owned()
        })?,
    };
    if requested.is_some() || firmware.is_update_for(installed)? {
        return Ok(Some(firmware));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn firmware(version: &str) -> Firmware {
        Firmware {
            version: version.into(),
            summary: String::new(),
            published: String::new(),
            size: 1,
            sha256: [0; 32],
        }
    }

    /// Automatic selection looks for an update; explicit published targets pass
    /// through even when they replace the same version or request a downgrade.
    #[test]
    fn test_selection() {
        let firmwares = [firmware("2.0.0-1234567"), firmware("1.0.0-develop")];
        assert_eq!(
            select(&firmwares, None, "1.0.0-fffffff")
                .unwrap()
                .unwrap()
                .version,
            "2.0.0-1234567"
        );
        assert!(select(&firmwares, None, "2.0.0-0000000").unwrap().is_none());
        assert!(select(&firmwares, None, "3.0.0-develop").unwrap().is_none());
        assert!(
            select(&firmwares, Some("1.0.0-develop"), "1.0.0-develop")
                .unwrap()
                .is_some()
        );
        for installed in ["1.0.0-1234567", "2.0.0-1234567"] {
            assert_eq!(
                select(&firmwares, Some("1.0.0-develop"), installed)
                    .unwrap()
                    .unwrap()
                    .version,
                "1.0.0-develop"
            );
        }
        assert!(select(&firmwares, Some("9.0.0-1234567"), "1.0.0-develop").is_err());
        assert!(select(&[], None, "1.0.0-develop").is_err());
    }
}
