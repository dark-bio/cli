// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use clap::{Parser, Subcommand};
use console::style;
use darkbio_connect::trust::Environment;
use darkbio_connect::{Ark, Device, Identity, Realm, TrustMode};
#[cfg(feature = "internal")]
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(name = "ark", about = "Command line interface for Ark enclaves")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the Ark enclaves plugged in and the emulators running
    List,

    /// Onboard an enclave with a signed attestation certificate
    #[cfg(feature = "internal")]
    Onboard {
        /// Path to the CWT attestation file
        #[arg(long)]
        cwt: PathBuf,

        /// Hex-encoded xDSA public key for recovery (bypasses CWT verification)
        #[arg(long)]
        pubkey: Option<String>,

        /// Serial, name or disk image of the enclave to use, when several are found
        #[arg(long)]
        device: Option<String>,
    },

    /// Query enclave identity and firmware information
    Status {
        /// Hex-encoded xDSA public key for recovery (bypasses CWT verification)
        #[arg(long)]
        pubkey: Option<String>,

        /// Serial, name or disk image of the enclave to use, when several are found
        #[arg(long)]
        device: Option<String>,
    },
}

/// CLI entry point. Parses arguments and dispatches to the appropriate command.
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::List => cmd_list(),
        #[cfg(feature = "internal")]
        Command::Onboard {
            cwt,
            pubkey,
            device,
        } => {
            let trust = parse_trust_mode(pubkey);
            let (ark, _) = open_enclave(device.as_deref(), &trust);
            cmd_onboard(&ark, &cwt);

            // Reconnect and print the status for immediate visual feedback, the
            // fresh handshake presenting the injected attestation. Onboarding
            // already succeeded, so a failure here only warns instead of failing
            // the command.
            drop(ark);
            match find_enclave(device.as_deref()).and_then(|device| {
                device
                    .connect(&TrustMode::RootOrSelf)
                    .map_err(|err| err.to_string())
            }) {
                Ok((ark, identity)) => {
                    println!();
                    cmd_status(&ark, &identity);
                }
                Err(err) => eprintln!(
                    "{} could not read status after onboarding: {}",
                    style("warning:").yellow().bold(),
                    err,
                ),
            }
        }
        Command::Status { pubkey, device } => {
            let trust = parse_trust_mode(pubkey);
            let (ark, identity) = open_enclave(device.as_deref(), &trust);
            cmd_status(&ark, &identity);
        }
    }
}

/// Parses an optional hex-encoded xDSA public key into a TrustMode. If a key is
/// provided, returns Recover mode; otherwise returns the default RootOrSelf.
fn parse_trust_mode(pubkey: Option<String>) -> TrustMode {
    match pubkey {
        None => TrustMode::RootOrSelf,
        Some(hex_key) => {
            let bytes = hex::decode(&hex_key).unwrap_or_else(|err| {
                eprintln!(
                    "{} invalid --pubkey hex: {}",
                    style("error:").red().bold(),
                    err,
                );
                process::exit(1);
            });
            let key_bytes: [u8; darkbio_crypto::xdsa::PUBLIC_KEY_SIZE] =
                bytes.as_slice().try_into().unwrap_or_else(|_| {
                    eprintln!(
                        "{} invalid --pubkey length (expected {} bytes, got {})",
                        style("error:").red().bold(),
                        darkbio_crypto::xdsa::PUBLIC_KEY_SIZE,
                        bytes.len(),
                    );
                    process::exit(1);
                });
            let key =
                darkbio_crypto::xdsa::PublicKey::from_bytes(&key_bytes).unwrap_or_else(|err| {
                    eprintln!("{} invalid --pubkey: {}", style("error:").red().bold(), err);
                    process::exit(1);
                });
            TrustMode::Recover(Box::new(key))
        }
    }
}

/// Finds the enclave to use, the only one found or the one selected by its
/// serial, name, disk image or label as listed. A selector several enclaves
/// match is refused rather than resolved to the first of them.
fn find_enclave(selector: Option<&str>) -> Result<Device, String> {
    let devices = darkbio_connect::list().map_err(|err| err.to_string())?;
    let Some(selector) = selector else {
        let mut devices = devices.into_iter();
        return match (devices.next(), devices.next()) {
            (None, _) => Err("no Ark enclave found".into()),
            (Some(device), None) => Ok(device),
            _ => Err("multiple Ark enclaves found, use --device to select one".into()),
        };
    };
    let mut matches: Vec<Device> = devices
        .into_iter()
        .filter(|device| {
            [device.serial(), device.name(), device.image()]
                .into_iter()
                .flatten()
                .any(|facet| facet == selector)
                || device.to_string() == selector
        })
        .collect();
    match matches.len() {
        0 => Err(format!("no Ark enclave matches {selector}")),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "{selector} matches several Ark enclaves, name one to tell them apart: {}",
            matches
                .iter()
                .map(|device| format!("{device} ({})", notes(device)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Locates the enclave to use and opens an encrypted connection. Exits on error.
fn open_enclave(selector: Option<&str>, trust: &TrustMode) -> (Ark, Identity) {
    let device = find_enclave(selector).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(1);
    });
    device.connect(trust).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    })
}

/// Name of an environment as printed.
fn environment_name(environment: Environment) -> &'static str {
    match environment {
        Environment::Release => "release",
        Environment::Staging => "staging",
        Environment::Develop => "develop",
    }
}

/// What is known of an enclave before connecting, how it is reached, the
/// environment it says it is bound to and whether it is still booting.
fn notes(device: &Device) -> String {
    let mut notes = vec![match device.realm() {
        Realm::Live => "hardware",
        Realm::Sandbox => "emulator",
    }];
    if let Some(environment) = device.environment() {
        notes.push(environment_name(environment));
    }
    if !device.ready() {
        notes.push("booting");
    }
    notes.join(", ")
}

/// Lists the enclaves plugged in and the emulators running, each by its label
/// with what is known of it before connecting.
fn cmd_list() {
    let devices = darkbio_connect::list().unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(1);
    });
    if devices.is_empty() {
        println!("No Ark enclaves found.");
        return;
    }
    for device in &devices {
        println!(
            "{} {}",
            style(device).bold(),
            style(format!("({})", notes(device))).dim(),
        );
    }
}

/// Reads a CWT attestation file and sends it to the enclave for onboarding.
#[cfg(feature = "internal")]
fn cmd_onboard(ark: &Ark, cwt_path: &PathBuf) {
    let cwt = std::fs::read(cwt_path).unwrap_or_else(|err| {
        eprintln!(
            "{} failed to read {}: {}",
            style("error:").red().bold(),
            cwt_path.display(),
            err
        );
        process::exit(1);
    });
    ark.onboard(cwt).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    println!("{}", style("Enclave onboarded successfully.").green());
}

/// Retrieves the device info and prints hardware, firmware and identity information.
fn cmd_status(ark: &Ark, identity: &Identity) {
    let info = ark.device_info().unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    use chrono::{Local, TimeZone};

    let published = Local
        .timestamp_opt(info.firmware_publish as i64, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S %Z");

    // Cross-check the attested claims against device-reported values (only for
    // root-signed)
    let (environment, device) = match identity {
        Identity::Attested {
            environment,
            device,
        } => (Some(*environment), Some(device)),
        _ => (None, None),
    };
    let fingerprint = identity.key().fingerprint();
    let reported_hw = format!("{} - {}", info.version_str, info.revision_str);
    let hw_mismatch = device.is_some_and(|device| device.version != reported_hw);

    // Hardware
    match device {
        Some(device) => {
            let model = String::from_utf8(device.model.clone())
                .unwrap_or_else(|err| format!("0x{}", hex::encode(err.into_bytes())));

            if hw_mismatch {
                println!(
                    "{} {} ({}) ({})",
                    style("Hardware: ").dim(),
                    reported_hw,
                    style(&model).dim(),
                    style(format!("certificate contains \"{}\"", device.version)).red(),
                );
            } else {
                println!(
                    "{} {} ({})",
                    style("Hardware: ").dim(),
                    reported_hw,
                    style(&model).dim(),
                );
            }
        }
        None => println!("{} {}", style("Hardware: ").dim(), reported_hw),
    }
    // Serial
    match device {
        Some(device) => println!("{} {}", style("Serial:   ").dim(), device.serial),
        None => println!(
            "{} {}",
            style("Serial:   ").dim(),
            style("not onboarded").red(),
        ),
    }

    // Genuine
    match environment {
        Some(Environment::Release) => {
            println!("{} {}", style("Genuine:  ").dim(), style("genuine").green());
        }
        Some(Environment::Staging) => {
            println!(
                "{} {} ({})",
                style("Genuine:  ").dim(),
                style("genuine").green(),
                style("staging").yellow(),
            );
        }
        Some(Environment::Develop) => {
            println!(
                "{} {} ({})",
                style("Genuine:  ").dim(),
                style("genuine").green(),
                style("develop").red(),
            );
        }
        None => {
            println!(
                "{} {}",
                style("Genuine:  ").dim(),
                style("not genuine").red(),
            );
        }
    }
    // Firmware
    println!(
        "{} {} ({})",
        style("Firmware: ").dim(),
        info.firmware_version,
        style(published).dim()
    );
    // Identity
    println!(
        "{} {}",
        style("Identity: ").dim(),
        hex::encode(fingerprint.to_bytes())
    );
    println!(
        "{} {}",
        style("Pubkey:   ").dim(),
        hex::encode(identity.key().to_bytes())
    );
}
