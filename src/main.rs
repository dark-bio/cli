// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Command dispatch, endpoint selection and terminal output for the Ark CLI.

mod progress;
mod slots;
mod update;
mod upload;

use clap::{Parser, Subcommand};
use console::style;
use darkbio_connect::schema::{DeviceInfoRequest, DeviceInfoResponse, UnlockRequest};
use darkbio_connect::trust::{Environment, Realm};
use darkbio_connect::wire::{protocol, transport};
use darkbio_connect::{Ark, Device, DeviceKind, Discovery, Identity, TrustMode};
#[cfg(feature = "internal")]
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "ark", about = "Command line interface for Ark enclaves")]
struct Cli {
    /// Cloud environment override (requires the matching build feature)
    #[arg(long, global = true, value_parser = parse_env)]
    env: Option<Environment>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List hardware Arks and running emulators
    List,

    /// Upload a dataset file or download a reference dataset to the Ark
    Upload(upload::Args),

    /// List the Ark's dataset slots and metadata
    Slots {
        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,

        /// Total budget in seconds for cloud setup and the slot request
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },

    /// Install published firmware and reboot the Ark
    Update {
        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,

        /// Published version to install (defaults to the newest build)
        #[arg(long)]
        version: Option<String>,

        /// Show the available update without changing the Ark
        #[arg(long)]
        check: bool,

        /// Total budget in seconds for approval, transfer and installation
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },

    /// Unlock the Ark with approval from its paired companion app
    Unlock {
        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,

        /// Total budget in seconds for cloud setup and companion approval
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },

    /// Verify the Ark against the cloud device registry
    Genuine {
        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,
    },

    /// Onboard an enclave with a signed attestation certificate
    #[cfg(feature = "internal")]
    Onboard {
        /// Path to the CWT attestation file
        #[arg(long)]
        cwt: PathBuf,

        /// Hex-encoded xDSA public key for recovery (bypasses CWT verification)
        #[arg(long)]
        pubkey: Option<String>,

        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,
    },

    /// Query enclave identity and firmware information
    Status {
        /// Hex-encoded xDSA public key for recovery (bypasses CWT verification)
        #[arg(long)]
        pubkey: Option<String>,

        /// Endpoint locator, or a unique serial, name or disk image
        #[arg(long)]
        device: Option<String>,
    },
}

/// Command failure carried to the entry point for printing and exit status.
#[derive(Debug)]
struct Error {
    code: u8,        // Process exit status for this class of failure
    message: String, // Diagnostic printed once by main
}

impl From<String> for Error {
    /// Reports local input and selection failures with exit status 1.
    fn from(message: String) -> Self {
        Self { code: 1, message }
    }
}

impl From<darkbio_connect::Error> for Error {
    /// Reports connection and request failures with exit status 3.
    fn from(error: darkbio_connect::Error) -> Self {
        let message = if let darkbio_connect::Error::Handshake(protocol::Error::Transport(cause)) =
            &error
            && let transport::Error::HandshakeFailed(reason) = cause.as_ref()
        {
            reason.clone()
        } else {
            match error {
                darkbio_connect::Error::MissingEnvironment => {
                    "cloud environment unknown; select one with --env".into()
                }
                error => error.to_string(),
            }
        };
        Self { code: 3, message }
    }
}

/// Parses the command and returns its exit status after printing any failure.
fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command, cli.env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{} {}", style("error:").red().bold(), error.message);
            ExitCode::from(error.code)
        }
    }
}

/// Runs the selected command, retaining successful onboarding even when its
/// subsequent status query fails.
fn run(command: Command, env: Option<Environment>) -> Result<(), Error> {
    match command {
        Command::Upload(args) => upload::run(args, env)?,
        Command::Slots { device, timeout } => {
            slots::run(device.as_deref(), env, timeout)?;
        }
        Command::Update {
            device,
            version,
            check,
            timeout,
        } => {
            update::run(device.as_deref(), env, version.as_deref(), check, timeout)?;
        }
        Command::List => {
            let found = discover()?;
            if found.devices.is_empty() {
                println!("No Ark enclaves found.");
            }
            for device in found.devices {
                println!(
                    "{}  {} {}",
                    device.locator(),
                    style(&device).bold(),
                    style(format!("({})", notes(&device))).dim(),
                );
            }
        }
        Command::Status { pubkey, device } => {
            let trust = parse_trust_mode(pubkey.as_deref())?;
            let endpoint = find_enclave(device.as_deref())?;
            let (ark, identity) = connect(&endpoint, &trust, env)?;
            println!("{}", status(&ark, &identity)?);
        }
        Command::Unlock { device, timeout } => {
            let endpoint = find_enclave(device.as_deref())?;
            let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
            eprintln!(
                "{}",
                style("Requesting unlock. Approve it in your companion app.").dim()
            );
            ark.client()
                .call_timeout(UnlockRequest {}, Duration::from_secs(timeout))?;
            println!("{}", style("Enclave unlocked successfully.").green());
        }
        Command::Genuine { device } => {
            let endpoint = find_enclave(device.as_deref())?;
            let (ark, _) = connect(&endpoint, &TrustMode::RootOrSelf, env)?;
            let registration = ark
                .client()
                .genuine(Instant::now() + Duration::from_secs(30))?;
            println!("{}", render_registration(&registration));
            if !registration.active() {
                return Err(Error {
                    code: 3,
                    message: "Ark registration is inactive".into(),
                });
            }
        }
        #[cfg(feature = "internal")]
        Command::Onboard {
            cwt,
            pubkey,
            device,
        } => {
            let trust = parse_trust_mode(pubkey.as_deref())?;
            // Refuse a missing certificate before selecting or opening a device.
            let certificate = std::fs::read(&cwt)
                .map_err(|error| format!("failed to read {}: {error}", cwt.display()))?;
            let endpoint = find_enclave(device.as_deref())?;
            let (ark, _) = connect(&endpoint, &trust, env)?;
            ark.client().call_timeout(
                darkbio_connect::schema::OnboardingRequest {
                    device_attestation: certificate,
                },
                Duration::from_secs(10),
            )?;
            println!("{}", style("Enclave onboarded successfully.").green());
            drop(ark);

            // Reuse the selected endpoint. Names and discovery metadata can change
            // after onboarding; neither reconnection nor status failure undoes it.
            let feedback = connect(&endpoint, &TrustMode::RootOrSelf, env)
                .and_then(|(ark, identity)| status(&ark, &identity));
            match feedback {
                Ok(status) => println!("\n{status}"),
                Err(error) => warn(format!("could not read status after onboarding: {error}")),
            }
        }
    }
    Ok(())
}

/// Selects cloud routing independently of the handshake's trust policy.
fn connect(
    endpoint: &Device,
    trust: &TrustMode,
    env: Option<Environment>,
) -> Result<(Ark, Identity), darkbio_connect::Error> {
    match env {
        Some(env) => endpoint.connect_with_env(trust, env),
        None => endpoint.connect(trust),
    }
}

/// Accepts only environments enabled by this build's features.
fn parse_env(value: &str) -> Result<Environment, String> {
    match value {
        #[cfg(feature = "release")]
        "release" => Ok(Environment::Release),
        #[cfg(feature = "staging")]
        "staging" => Ok(Environment::Staging),
        #[cfg(feature = "develop")]
        "develop" => Ok(Environment::Develop),
        _ if matches!(value, "release" | "staging" | "develop") => Err(format!(
            "cloud environment {value} is disabled; build with --features {value}"
        )),
        _ => Err("expected release, staging or develop".into()),
    }
}

/// Selects root verification or decodes the identity key supplied for recovery.
fn parse_trust_mode(pubkey: Option<&str>) -> Result<TrustMode, String> {
    let Some(encoded) = pubkey else {
        return Ok(TrustMode::RootOrSelf);
    };
    let bytes = hex::decode(encoded).map_err(|error| format!("invalid --pubkey hex: {error}"))?;
    let key_bytes = bytes.as_slice().try_into().map_err(|_| {
        format!(
            "invalid --pubkey length (expected {} bytes, got {})",
            darkbio_crypto::xdsa::PUBLIC_KEY_SIZE,
            bytes.len(),
        )
    })?;
    let key = darkbio_crypto::xdsa::PublicKey::from_bytes(key_bytes)
        .map_err(|error| format!("invalid --pubkey: {error}"))?;
    Ok(TrustMode::Recover(Box::new(key)))
}

/// Prints a diagnostic that does not change the command's successful outcome.
fn warn(message: impl std::fmt::Display) {
    eprintln!("{} {message}", style("warning:").yellow().bold());
}

/// Lists endpoints and reports individual source failures. An empty, incomplete
/// discovery is a failure rather than confirmation that no devices are present.
fn discover() -> Result<Discovery, String> {
    let found = darkbio_connect::list();
    for error in &found.errors {
        warn(error);
    }
    if found.devices.is_empty() && !found.errors.is_empty() {
        return Err("no Ark enclave found; discovery was incomplete".into());
    }
    Ok(found)
}

/// Selects one endpoint and includes candidate locators in ambiguity diagnostics.
fn find_enclave(selector: Option<&str>) -> Result<Device, String> {
    let found = discover()?;
    found
        .select(selector)
        .cloned()
        .map_err(|error| match error {
            darkbio_connect::Error::Ambiguous(locators) => format!(
                "multiple Ark enclaves match; use --device with one of: {}",
                locators
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            error => error.to_string(),
        })
}

/// Highlights internal environments consistently with the status output.
fn env_style(env: Environment) -> console::Style {
    match env {
        Environment::Release => console::Style::new().dim(),
        Environment::Staging => console::Style::new().yellow(),
        Environment::Develop => console::Style::new().red(),
    }
}

/// Formats discovery details and launcher reports as observations.
fn notes(device: &Device) -> String {
    let mut notes = vec![match device.kind() {
        DeviceKind::Hardware => "hardware".to_owned(),
        DeviceKind::Emulator => "emulator".to_owned(),
    }];
    if let Some(env) = device.env() {
        notes.push(format!("reported environment: {env}"));
    }
    match device.ready() {
        Some(true) => notes.push("reported ready".into()),
        Some(false) => notes.push("reported booting".into()),
        None => {}
    }
    notes.join(", ")
}

/// Retrieves device information and formats it beside the authenticated identity.
fn status(ark: &Ark, identity: &Identity) -> Result<String, darkbio_connect::Error> {
    let info = ark
        .client()
        .call_timeout(DeviceInfoRequest {}, Duration::from_secs(10))?;
    Ok(render_status(&info, identity))
}

/// Formats a verified registry entry and every reason it may be inactive.
fn render_registration(registration: &darkbio_connect::Registration) -> String {
    use chrono::{Local, TimeZone};

    let enrolled = Local
        .timestamp_opt(registration.enrolled, 0)
        .single()
        .map(|date| date.format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| "invalid enrollment timestamp".into());
    let mut inactive = Vec::new();
    if registration.disabled {
        inactive.push("disabled");
    }
    if registration.expired {
        inactive.push("expired");
    }
    if registration.superseded {
        inactive.push("superseded");
    }
    let state = if inactive.is_empty() {
        style("active".to_owned()).green()
    } else {
        style(inactive.join(", ")).yellow()
    };
    format!(
        "{} {}\n{} {}\n{} {}",
        style("Serial:   ").dim(),
        registration.serial,
        style("Enrolled: ").dim(),
        style(enrolled).dim(),
        style("Registry: ").dim(),
        state,
    )
}

/// Formats reported hardware and firmware beside the claims established by the
/// handshake. Flags certificate mismatches and invalid publication timestamps.
fn render_status(info: &DeviceInfoResponse, identity: &Identity) -> String {
    use chrono::{Local, TimeZone};

    // Peer timestamps may exceed either the integer range or chrono's date range.
    let published = i64::try_from(info.firmware_publish)
        .ok()
        .and_then(|timestamp| Local.timestamp_opt(timestamp, 0).single())
        .map(|date| date.format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| "invalid publication timestamp".into());
    let reported_hw = format!("{} - {}", info.version_str, info.revision_str);
    // Only a trusted attestation supplies the serial and realm. Self-signing
    // and recovery establish key possession without those certificate claims.
    let (hardware, serial, trust) = match identity {
        Identity::Attested { env, device } => {
            let model = String::from_utf8(device.model.clone())
                .unwrap_or_else(|error| format!("0x{}", hex::encode(error.into_bytes())));
            let mismatch = if device.version != reported_hw {
                format!(
                    " ({})",
                    style(format!("certificate contains \"{}\"", device.version)).red(),
                )
            } else {
                String::new()
            };
            let realm = match device.realm {
                Realm::Hardware => "hardware",
                Realm::Emulator => "emulator",
            };
            let env = env_style(*env).apply_to(env);
            (
                format!("{reported_hw} ({}){mismatch}", style(model).dim()),
                style(device.serial.as_str()),
                format!(
                    "{} ({env}, {})",
                    style("attested").green(),
                    style(realm).dim()
                ),
            )
        }
        Identity::SelfSigned(_) => (
            reported_hw,
            style("unverified").yellow(),
            style("self-signed; provisioning unverified")
                .yellow()
                .to_string(),
        ),
        Identity::Recovered(_) => (
            reported_hw,
            style("unverified").yellow(),
            style("pinned key; attestation not checked")
                .yellow()
                .to_string(),
        ),
    };
    format!(
        "{} {hardware}\n\
         {} {serial}\n\
         {} {trust}\n\
         {} {} ({})\n\
         {} {}\n\
         {} {}",
        style("Hardware: ").dim(),
        style("Serial:   ").dim(),
        style("Trust:    ").dim(),
        style("Firmware: ").dim(),
        info.firmware_version,
        style(published).dim(),
        style("Identity: ").dim(),
        hex::encode(identity.key().fingerprint().to_bytes()),
        style("Pubkey:   ").dim(),
        hex::encode(identity.key().to_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cloud selection works before or after the command and refuses environments
    /// disabled at build time. Enabling a feature does not select its cloud.
    #[test]
    fn test_cloud_environment() {
        assert!(Cli::try_parse_from(["ark", "slots"]).unwrap().env.is_none());
        for (env, enabled) in [
            ("release", cfg!(feature = "release")),
            ("staging", cfg!(feature = "staging")),
            ("develop", cfg!(feature = "develop")),
        ] {
            for args in [
                ["ark", "--env", env, "slots"],
                ["ark", "slots", "--env", env],
            ] {
                match Cli::try_parse_from(args) {
                    Ok(cli) => {
                        assert!(enabled);
                        assert_eq!(cli.env.map(|env| env.to_string()).as_deref(), Some(env));
                    }
                    Err(error) => {
                        assert!(!enabled);
                        assert!(error.to_string().contains(&format!("--features {env}")));
                    }
                }
            }
        }
        assert!(Cli::try_parse_from(["ark", "slots", "--env", "unknown"]).is_err());
        let error = Error::from(darkbio_connect::Error::MissingEnvironment);
        assert!(error.message.contains("--env"));
    }

    /// Registry output preserves all inactive states and handles an invalid
    /// timestamp without obscuring the verified serial or reporting active.
    #[test]
    fn test_registry_status() {
        let mut registration = darkbio_connect::Registration {
            serial: "verified-serial".into(),
            enrolled: 1_700_000_000,
            disabled: false,
            expired: false,
            superseded: false,
        };
        assert!(registration.active());
        let rendered = render_registration(&registration);
        let text = console::strip_ansi_codes(&rendered);
        assert!(text.contains("verified-serial"));
        assert!(text.contains("active"));

        registration.enrolled = i64::MAX;
        registration.disabled = true;
        registration.expired = true;
        registration.superseded = true;
        assert!(!registration.active());
        let rendered = render_registration(&registration);
        let text = console::strip_ansi_codes(&rendered);
        assert!(text.contains("invalid enrollment timestamp"));
        assert!(text.contains("disabled, expired, superseded"));
    }

    /// Status uses the certificate's realm and reports mismatched hardware claims.
    #[cfg(any(feature = "release", feature = "staging", feature = "develop"))]
    #[test]
    fn test_attested_status() {
        #[cfg(feature = "release")]
        let env = Environment::Release;
        #[cfg(all(not(feature = "release"), feature = "staging"))]
        let env = Environment::Staging;
        #[cfg(all(
            not(feature = "release"),
            not(feature = "staging"),
            feature = "develop"
        ))]
        let env = Environment::Develop;
        let identity = Identity::Attested {
            env,
            device: darkbio_connect::trust::device::Device {
                realm: Realm::Emulator,
                identity: darkbio_crypto::xdsa::SecretKey::generate().public_key(),
                oem: darkbio_crypto::cwt::claims::eat::Oemid::new_pen(0),
                serial: "verified-serial".into(),
                model: vec![0xff],
                version: "certified revision".into(),
                issued: 0,
                expiry: Some(1),
            },
        };
        let rendered = render_status(&DeviceInfoResponse::default(), &identity);
        let status = console::strip_ansi_codes(&rendered);
        assert_eq!(identity.realm(), Some(Realm::Emulator));
        assert!(status.contains(&format!("attested ({}, emulator)", env)));
        assert!(status.contains("verified-serial"));
        assert!(status.contains("0xff"));
        assert!(status.contains("certificate contains \"certified revision\""));
    }

    /// Recovery and self-signing do not establish provisioning history. Invalid
    /// firmware timestamps remain printable.
    #[test]
    fn test_unverified_status() {
        let key = darkbio_crypto::xdsa::SecretKey::generate().public_key();
        let info = DeviceInfoResponse {
            firmware_publish: u64::MAX,
            ..Default::default()
        };
        let recovered = render_status(&info, &Identity::Recovered(key.clone()));
        assert!(recovered.contains("pinned key; attestation not checked"));
        assert!(recovered.contains("invalid publication timestamp"));
        let self_signed = render_status(&info, &Identity::SelfSigned(key));
        assert!(self_signed.contains("self-signed; provisioning unverified"));
        for output in [recovered, self_signed] {
            assert!(!output.contains("not onboarded"));
            assert!(!output.contains("not genuine"));
        }
    }
}
