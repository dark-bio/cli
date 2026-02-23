// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

mod enclave;
mod wire;
mod wire_protocol;

use clap::{Parser, Subcommand};
use console::style;
use nusb::MaybeFuture;
#[cfg(feature = "internal")]
use std::path::PathBuf;
use std::process;

use enclave::Enclave;

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
    },

    /// Query enclave identity and firmware information
    Status,
}

/// CLI entry point. Parses arguments and dispatches to the appropriate command.
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::List => cmd_list(),
        #[cfg(feature = "internal")]
        Command::Onboard { cwt } => {
            let mut enc = open_enclave();
            cmd_onboard(&mut enc, &cwt);
        }
        Command::Status => {
            let mut enc = open_enclave();
            cmd_status(&mut enc);
        }
    }
}

/// Locates exactly one Ark enclave and opens a connection. Exits on error.
fn open_enclave() -> Enclave {
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
    Enclave::open(&devices[0]).unwrap_or_else(|err| {
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
    let info = enc.handshake().unwrap_or_else(|err| {
        eprintln!("{} {}", style("error:").red().bold(), err);
        process::exit(3);
    });
    use chrono::{Local, TimeZone};

    let published = Local
        .timestamp_opt(info.firmware_publish as i64, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S %Z");

    println!(
        "{} {} - {}",
        style("Hardware: ").dim(),
        info.version_str,
        info.revision_str
    );
    println!(
        "{} {} ({})",
        style("Firmware: ").dim(),
        info.firmware_version,
        style(published).dim()
    );
    if !info.compute_identity.is_empty() {
        let hex: String = info
            .compute_identity
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        println!("{} {}", style("Identity: ").dim(), hex);
    }
}
