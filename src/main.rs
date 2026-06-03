// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

mod attests;
mod enclave;
mod pubkeys;
mod wire;
mod wire_protocol;

use clap::{Parser, Subcommand};
use console::style;
use nusb::MaybeFuture;
#[cfg(feature = "internal")]
use std::path::PathBuf;
use std::process;

use enclave::Enclave;
use pubkeys::Release;
use wire::TrustMode;

/// USB VID:PID for Dark Bio Ark enclaves.
const ARK_VID: u16 = 0x2e8a;
const ARK_PID: u16 = 0x10f1;

#[derive(Parser)]
#[command(name = "ark", about = "Command line interface for Ark enclaves")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List all connected Ark enclaves
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
    },

    /// Query enclave identity and firmware information
    Status {
        /// Hex-encoded xDSA public key for recovery (bypasses CWT verification)
        #[arg(long)]
        pubkey: Option<String>,
    },
}

/// CLI entry point. Parses arguments and dispatches to the appropriate command.
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::List => cmd_list(),
        #[cfg(feature = "internal")]
        Command::Onboard { cwt, pubkey } => {
            let trust = parse_trust_mode(pubkey);
            let mut enc = open_enclave(trust);
            cmd_onboard(&mut enc, &cwt);

            // Refresh the session and print the status for immediate visual
            // feedback. Onboarding already succeeded, so a status read failure
            // only warns instead of failing the command.
            match enc.refresh_session(TrustMode::RootOrSelf) {
                Ok(()) => {
                    println!();
                    cmd_status(&mut enc);
                }
                Err(err) => eprintln!(
                    "{} could not read status after onboarding: {}",
                    style("warning:").yellow().bold(),
                    err,
                ),
            }
        }
        Command::Status { pubkey } => {
            let trust = parse_trust_mode(pubkey);
            let mut enc = open_enclave(trust);
            cmd_status(&mut enc);
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
            TrustMode::Recover(key)
        }
    }
}

/// Locates exactly one Ark enclave and opens an encrypted connection. Exits on error.
fn open_enclave(trust: TrustMode) -> Enclave {
    let devices: Vec<_> = nusb::list_devices()
        .wait()
        .expect("failed to enumerate USB devices")
        .filter(|d| d.vendor_id() == ARK_VID && d.product_id() == ARK_PID)
        .collect();

    if devices.is_empty() {
        eprintln!("{} no Ark enclave found", style("error:").red().bold());
        process::exit(1);
    }
    if devices.len() > 1 {
        eprintln!(
            "{} multiple Ark enclaves found, use --serial to select one",
            style("error:").red().bold()
        );
        process::exit(2);
    }
    Enclave::open(&devices[0], trust).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    })
}

/// Lists all connected Ark enclaves with their product name and serial number.
fn cmd_list() {
    let arks: Vec<_> = nusb::list_devices()
        .wait()
        .expect("failed to enumerate USB devices")
        .filter(|d| d.vendor_id() == ARK_VID && d.product_id() == ARK_PID)
        .collect();

    if arks.is_empty() {
        println!("No Ark enclaves found.");
        return;
    }
    for (i, dev) in arks.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let serial = dev.serial_number().unwrap_or("unknown");
        let product = dev.product_string().unwrap_or("Ark");

        println!(
            "{} {}",
            style(product).bold(),
            style(format!("({})", serial)).dim(),
        );
    }
}

/// Reads a CWT attestation file and sends it to the enclave for onboarding.
#[cfg(feature = "internal")]
fn cmd_onboard(enc: &mut Enclave, cwt_path: &PathBuf) {
    let cwt = std::fs::read(cwt_path).unwrap_or_else(|err| {
        eprintln!(
            "{} failed to read {}: {}",
            style("error:").red().bold(),
            cwt_path.display(),
            err
        );
        process::exit(1);
    });
    enc.onboard(&cwt).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    println!("{}", style("Enclave onboarded successfully.").green());
}

/// Performs a handshake and prints hardware, firmware and identity information.
fn cmd_status(enc: &mut Enclave) {
    let serial = enc.session_info().serial.clone();
    let hw_model = enc.session_info().hw_model.clone();
    let hw_version = enc.session_info().hw_version.clone();
    let release = enc.session_info().release;
    let cwt_fingerprint = enc.session_info().ark_identity.fingerprint();
    let info = enc.handshake().unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    use chrono::{Local, TimeZone};

    let published = Local
        .timestamp_opt(info.firmware_publish as i64, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S %Z");

    // Cross-check CWT claims against device-reported values (only for root-signed)
    let root_signed = release.is_some();
    let reported_hw = format!("{} - {}", info.version_str, info.revision_str);
    let hw_mismatch = root_signed && hw_version != reported_hw;
    let id_mismatch = root_signed
        && !info.compute_identity.is_empty()
        && info.compute_identity != cwt_fingerprint.to_bytes().as_slice();

    // Hardware
    let hw_model_str = String::from_utf8(hw_model)
        .unwrap_or_else(|err| format!("0x{}", hex::encode(err.into_bytes())));

    if hw_mismatch {
        println!(
            "{} {} - {} ({}) ({})",
            style("Hardware: ").dim(),
            info.version_str,
            info.revision_str,
            style(&hw_model_str).dim(),
            style(format!("certificate contains \"{}\"", hw_version)).red(),
        );
    } else {
        println!(
            "{} {} - {} ({})",
            style("Hardware: ").dim(),
            info.version_str,
            info.revision_str,
            style(&hw_model_str).dim(),
        );
    }
    // Serial
    if root_signed {
        println!("{} {}", style("Serial:   ").dim(), serial);
    } else {
        println!(
            "{} {}",
            style("Serial:   ").dim(),
            style("not onboarded").red(),
        );
    }

    // Genuine
    match release {
        Some(Release::Release) => {
            println!("{} {}", style("Genuine:  ").dim(), style("genuine").green());
        }
        Some(Release::Staging) => {
            println!(
                "{} {} ({})",
                style("Genuine:  ").dim(),
                style("genuine").green(),
                style("staging").yellow(),
            );
        }
        Some(Release::Develop) => {
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
    if !info.compute_identity.is_empty() {
        let hex: String = info
            .compute_identity
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        if id_mismatch {
            let cwt_hex: String = cwt_fingerprint
                .to_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!(
                "{} {} ({})",
                style("Identity: ").dim(),
                hex,
                style(format!("certificate contains \"{}\"", cwt_hex)).red(),
            );
        } else {
            println!("{} {}", style("Identity: ").dim(), hex);
        }
    }
    // Public key
    let pubkey_bytes: [u8; darkbio_crypto::xdsa::PUBLIC_KEY_SIZE] = info
        .compute_pubkey
        .as_slice()
        .try_into()
        .unwrap_or_else(|_| {
            eprintln!(
                "{} invalid public key length (expected {}, got {})",
                style("error:").red().bold(),
                darkbio_crypto::xdsa::PUBLIC_KEY_SIZE,
                info.compute_pubkey.len(),
            );
            process::exit(3);
        });
    let pubkey = darkbio_crypto::xdsa::PublicKey::from_bytes(&pubkey_bytes).unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    println!(
        "{} {}",
        style("Pubkey:   ").dim(),
        hex::encode(&info.compute_pubkey)
    );
    println!(
        "{} {}",
        style("Fingerp.: ").dim(),
        hex::encode(pubkey.fingerprint().to_bytes())
    );
}
