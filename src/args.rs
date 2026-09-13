// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Command names, arguments and shared options.
//!
//! Clap consumes doc comments on arguments and variants as public help text.
//! Developer-only notes beside those declarations use ordinary comments.

use clap::{Args, Parser, Subcommand, ValueEnum};
use darkbio_connect::trust::Environment;
use std::path::PathBuf;

// Parsed command and global options; cross-level conflicts are checked afterward.
#[derive(Parser)]
#[command(
    name = "ark",
    about = "Command line interface to Ark enclaves",
    disable_help_subcommand = true,
    disable_help_flag = true,
    disable_version_flag = true,
    propagate_version = false
)]
pub(crate) struct Cli {
    // Options propagated to every command level by clap.
    #[command(flatten)]
    pub options: Options,
    /// Tool, connect and wire versions
    #[arg(short = 'V', long)]
    pub version: bool,
    /// Print help; `ark help --all` prints the manual
    #[arg(short = 'h', long)]
    pub help: bool,
    // Accept the common --help --all spelling of the manual.
    #[arg(long, hide = true, requires = "help", conflicts_with = "version")]
    pub all: bool,
    // Absent for top-level help or the standalone version flag.
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Clap checks conflicts within one parser level. Global values may have
    /// been supplied at an ancestor, so verify these after propagation too.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        let dry = matches!(
            &self.command,
            Some(Command::Data(
                Data::Upload { dry_run: true, .. }
                    | Data::Fetch { dry_run: true, .. }
                    | Data::Delete(Change { dry_run: true, .. })
                    | Data::Repair(Change { dry_run: true, .. })
            ))
        ) | matches!(
            &self.command,
            Some(Command::Firmware(Firmware::Update { dry_run: true, .. }))
        );
        let message = if dry && self.options.unlock {
            Some("--dry-run cannot be combined with --unlock")
        } else if self.options.quiet && self.options.verbose {
            Some("--quiet cannot be combined with --verbose")
        } else if self.version && self.command.is_some() {
            Some("--version cannot be combined with a command")
        } else {
            None
        };
        message.map_or(Ok(()), |message| {
            Err(crate::error::Error::new(2, "usage", message))
        })
    }
}

// Invocation-wide presentation and device policy, independent of connect's API.
#[derive(Args, Clone)]
pub(crate) struct Options {
    /// Which Ark: locator, unique serial, name, image, or hardware/emulator
    #[arg(short = 'd', long, global = true, value_name = "SELECTOR")]
    pub device: Option<String>,
    /// Print the complete result as JSON and stderr events as JSON Lines
    #[arg(long, global = true)]
    pub json: bool,
    /// Seconds to wait for each reply or network chunk, not an approval or the whole run
    #[arg(long, global = true, default_value_t = 60, value_parser = parse_timeout, value_name = "SECONDS")]
    pub timeout: u64,
    /// Unlock first when needed, approved on your phone
    #[arg(long, global = true)]
    pub unlock: bool,
    /// Confirm firmware installation and reboot
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,
    /// Never prompt; fail with the flag needed to continue
    #[arg(long, global = true)]
    pub no_input: bool,
    /// Cloud environment; overriding a trusted attestation produces a warning
    #[arg(long, global = true, value_parser = parse_env)]
    pub env: Option<Environment>,
    /// Hide progress, notes and warnings; keep errors, hints and approvals
    #[arg(short = 'q', long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Show steps
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
    /// Diagnostic logs: debug for connect, trace for connect and wire
    #[arg(long, global = true, value_enum)]
    pub log: Option<Log>,
}

// Diagnostic detail, independent of step narration.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum Log {
    Debug,
    Trace,
}

// Top-level command palette, shared by parsing, help and shell completion.
#[derive(Subcommand)]
pub(crate) enum Command {
    /// Find hardware Arks and running emulators
    Devices,
    /// Show identity, trust, firmware, pairing and lock state
    Status(Recovery),
    /// Verify the Ark against Dark Bio's device registry
    Genuine,
    /// Pair the Ark with Ark Companion on your phone
    Pair,
    /// Unlock the Ark, approved on your phone
    Unlock,
    /// Give the Ark its attested identity
    Enroll(Enroll),
    /// Datasets: read with list/show/paths; change with upload/fetch/delete/repair
    #[command(subcommand)]
    Data(Data),
    /// Apps: run, cancel
    #[command(subcommand)]
    App(App),
    /// Firmware: list, update
    #[command(subcommand)]
    Firmware(Firmware),
    /// Check this computer, the Ark and the cloud; suggest fixes
    Doctor,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
    /// Help for a command or a topic; --all prints the manual
    Help {
        /// Command path or topic name
        #[arg(num_args = 0.., value_name = "COMMAND_OR_TOPIC")]
        path: Vec<String>,
        // Print all command help and embedded topics as one manual.
        #[arg(long, conflicts_with = "path")]
        all: bool,
    },
}

// Explicit identity pin accepted by diagnostics and enrollment recovery.
#[derive(Args)]
pub(crate) struct Recovery {
    /// Pin an xDSA public key instead of verifying the attestation
    #[arg(
        long,
        value_name = "HEX",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    pub pubkey: Option<String>,
}

// Local attestation input and optional identity pin for enrollment.
#[derive(Args)]
pub(crate) struct Enroll {
    /// Install an existing signed attestation
    #[arg(long, value_name = "FILE")]
    pub cwt: Option<PathBuf>,
    // Recovery can authenticate a device whose stored attestation is unusable.
    #[command(flatten)]
    pub recovery: Recovery,
}

// Dataset inspection and mutation commands; slot names map to wire IDs below.
#[derive(Subcommand)]
pub(crate) enum Data {
    /// Print the Ark's README of paths available to apps
    Paths,
    /// List dataset slots and their state
    List,
    /// Show one slot's metadata, download URL and dependencies
    Show {
        /// Slot name or id from `ark data list`
        #[arg(value_parser = parse_slot)]
        slot: i32,
    },
    /// Upload a local dataset, approved on your phone
    Upload {
        /// Local dataset file to identify and upload
        file: PathBuf,
        /// Require the Ark to identify this target slot
        #[arg(long, value_parser = parse_slot)]
        slot: Option<i32>,
        /// Identify and plan without uploading or unlocking
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
    },
    /// Download and install public reference data
    Fetch {
        /// Slot name or id from `ark data list`
        #[arg(value_parser = parse_slot, required_unless_present = "all", conflicts_with = "all")]
        slot: Option<i32>,
        /// Fill empty reference slots in dependency order
        #[arg(long)]
        all: bool,
        /// Show the download plan without changing the Ark
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
        /// Reference cache directory
        #[arg(long, value_name = "DIR", conflicts_with = "no_cache")]
        cache: Option<PathBuf>,
        /// Stream without retaining a local copy
        #[arg(long)]
        no_cache: bool,
    },
    /// Empty a filled slot, approved on your phone
    Delete(Change),
    /// Reset a slot to empty, approved on your phone
    Repair(Change),
}

// Shared selection and planning arguments for slot deletion and repair.
#[derive(Args)]
pub(crate) struct Change {
    /// Slot name or id from `ark data list`
    #[arg(value_parser = parse_slot)]
    pub slot: i32,
    /// Show state and dependents without changing the Ark
    #[arg(long, conflicts_with = "unlock")]
    pub dry_run: bool,
}

// App execution and explicit cancellation by the task ID returned by the Ark.
#[derive(Subcommand)]
pub(crate) enum App {
    /// Run an app, approved on your phone; print its report
    Run {
        /// Local WebAssembly app
        file: PathBuf,
    },
    /// Cancel a running app or unfinished upload
    Cancel {
        /// Task id printed by `ark app run`
        task: u64,
    },
}

// Published firmware selection and installation, including read-only planning.
#[derive(Subcommand)]
pub(crate) enum Firmware {
    /// Show the installed build and update candidates
    List,
    /// Install firmware and reboot the Ark
    Update {
        /// Ask the Ark to install this exact published build
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Plan the update without approval or installation
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
        /// Verify the Ark returns running the target build (default)
        #[arg(long, conflicts_with = "no_wait")]
        wait: bool,
        /// Return when installation is acknowledged
        #[arg(long)]
        no_wait: bool,
    },
}

/// Parses the three explicit cloud routes supported by the CLI.
pub(crate) fn parse_env(value: &str) -> Result<Environment, String> {
    match value {
        "release" => Ok(Environment::Release),
        "staging" => Ok(Environment::Staging),
        "develop" => Ok(Environment::Develop),
        _ => Err(format!(
            "unknown environment {value:?}; use release, staging or develop"
        )),
    }
}

/// Protocol names remain exact; numeric IDs keep future slots addressable.
pub(crate) fn parse_slot(value: &str) -> Result<i32, String> {
    if let Ok(id) = value.parse::<i32>()
        && id > 0
    {
        return Ok(id);
    }
    let name = format!("SLOT_{}", value.replace('-', "_").to_ascii_uppercase());
    darkbio_connect::schema::SlotKind::from_str_name(&name)
        .filter(|kind| *kind as i32 > 0 && slot_name(*kind as i32) == value)
        .map(i32::from)
        .ok_or_else(|| format!("unknown slot {value:?}; use a name or id from `ark data list`"))
}

/// Formats known protocol slot names, retaining unknown IDs as decimal selectors.
pub(crate) fn slot_name(id: i32) -> String {
    darkbio_connect::schema::SlotKind::try_from(id)
        .map(|kind| {
            kind.as_str_name()
                .trim_start_matches("SLOT_")
                .to_ascii_lowercase()
                .replace('_', "-")
        })
        .unwrap_or_else(|_| id.to_string())
}

/// Reject durations that cannot be represented as monotonic deadlines.
fn parse_timeout(value: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "timeout must be a positive number of seconds")?;
    if seconds == 0
        || std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(seconds))
            .is_none()
    {
        return Err("timeout must be a positive number of seconds".into());
    }
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_environments_parse() {
        assert!(
            parse_env("prod")
                .unwrap_err()
                .starts_with("unknown environment")
        );
        for (name, env) in [
            ("release", Environment::Release),
            ("staging", Environment::Staging),
            ("develop", Environment::Develop),
        ] {
            assert_eq!(parse_env(name), Ok(env));
            let cli = Cli::try_parse_from(["ark", "--env", name, "status"]).unwrap();
            assert_eq!(cli.options.env, Some(env));
        }
    }

    #[test]
    fn slots_are_exact_and_future_ids_remain_addressable() {
        for id in 1..=4 {
            assert_eq!(parse_slot(&slot_name(id)), Ok(id));
            assert_eq!(parse_slot(&id.to_string()), Ok(id));
        }
        assert_eq!(parse_slot("2147483647"), Ok(i32::MAX));
        for invalid in [
            "0",
            "unspecified",
            "-1",
            "SNP-INDEL-CALLS",
            "snp_indel_calls",
            "snp",
            "2147483648",
        ] {
            assert!(parse_slot(invalid).is_err());
        }
    }

    #[test]
    fn narration_and_diagnostics_are_independent() {
        let cli = Cli::try_parse_from(["ark", "-v", "status", "--log", "debug"]).unwrap();
        assert!(cli.options.verbose);
        assert_eq!(cli.options.log, Some(Log::Debug));
        let cli = Cli::try_parse_from(["ark", "--log", "trace", "status"]).unwrap();
        assert!(!cli.options.verbose);
        assert_eq!(cli.options.log, Some(Log::Trace));
        for args in [
            vec!["ark", "-vv"],
            vec!["ark", "-vvv"],
            vec!["ark", "--log", "info"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn parser_conflicts_protect_dry_runs_and_explicit_selection() {
        for args in [
            vec!["ark", "--unlock", "data", "delete", "1", "--dry-run"],
            vec!["ark", "data", "fetch", "--all", "1"],
            vec![
                "ark",
                "data",
                "fetch",
                "--all",
                "--cache",
                "x",
                "--no-cache",
            ],
            vec!["ark", "firmware", "update", "--wait", "--no-wait"],
            vec!["ark", "--quiet", "status", "-v"],
        ] {
            assert!(
                Cli::try_parse_from(&args).map_or(true, |cli| cli.validate().is_err()),
                "{args:?}"
            );
        }
        assert!(Cli::try_parse_from(["ark", "app", "cancel", "18446744073709551615"]).is_ok());
    }
}
