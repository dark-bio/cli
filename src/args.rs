// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Command names, arguments and shared options.
//!
//! Clap consumes doc comments on arguments and variants as public help text.
//! Developer-only notes beside those declarations use ordinary comments.

use crate::data::samples::{self, Sex};
use clap::{Args, Parser, Subcommand, ValueEnum};
use darkbio_connect::{schema::SlotKind, trust::Environment};
use std::path::PathBuf;

/// Lists an enum's variants, and fails to compile when the list misses one.
macro_rules! variants {
    ($kind:ty, $($variant:path),+ $(,)?) => {{
        /// Match that stops compiling when a variant is left out.
        const _: fn($kind) = |value| match value {
            $($variant => (),)+
        };
        [$($variant),+]
    }};
}

// Parsed command and global options; cross-level conflicts are checked afterward
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
    // Options propagated to every command level by clap
    #[command(flatten)]
    pub options: Options,
    /// Print the version
    #[arg(short = 'V', long)]
    pub version: bool,
    /// Print help; `ark help --all` prints the manual
    #[arg(short = 'h', long)]
    pub help: bool,
    // Accept the common --help --all spelling of the manual
    #[arg(long, hide = true, requires = "help", conflicts_with = "version")]
    pub all: bool,
    // Absent for top-level help or the standalone version flag
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Checks the flag rules clap does not express, failing with a usage error
    /// of class 2.
    ///
    /// Clap checks conflicts within one parser level, while a global flag may
    /// come from any level. This rejects `--dry-run` with `--unlock`, `--quiet`
    /// with `--verbose`, `--version` with a command, `--sex` on
    /// `ark data samples` without a population or `--all`, and `--sample` with
    /// a slot.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        // --sex narrows a list of people, which takes a population or --all
        if matches!(
            &self.command,
            Some(Command::Data(Data::Samples {
                population: None,
                all: false,
                sex: Some(_),
            }))
        ) {
            return Err(
                crate::error::Error::new(2, "usage", "--sex needs a population or --all")
                    .hint("see `ark data samples --help`"),
            );
        }

        // A sample fills every personal slot, so it names none
        if let Some(Command::Data(Data::Fetch {
            slot: Some(_),
            sample: Some(sample),
            ..
        })) = &self.command
        {
            return Err(
                crate::error::Error::new(2, "usage", "--sample takes no slot")
                    .hint(format!("run `ark data fetch --sample {}`", sample.name())),
            );
        }

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

// Invocation-wide presentation and device policy, independent of connect's API
#[derive(Args, Clone)]
pub(crate) struct Options {
    /// Which Ark: locator, serial, name, image or kind
    #[arg(short = 'd', long, global = true, value_name = "SELECTOR")]
    pub device: Option<String>,
    /// Print JSON, with events as JSON Lines on stderr
    #[arg(long, global = true)]
    pub json: bool,
    /// Unlock a locked Ark first, approved on your phone
    #[arg(long, global = true)]
    pub unlock: bool,
    /// Never prompt; fail naming the flag instead
    #[arg(long, global = true)]
    pub no_input: bool,
    /// Limit each wait on a machine
    #[arg(long, global = true, default_value_t = 60, value_parser = parse_timeout, value_name = "SECONDS")]
    pub timeout: u64,
    /// Cloud environment: release, staging or develop
    #[arg(long, global = true, value_parser = parse_env)]
    pub env: Option<Environment>,
    /// Hide progress, notes, warnings and steps
    #[arg(short = 'q', long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Show steps
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
    /// Print diagnostics: debug or trace
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "LEVEL",
        hide_possible_values = true
    )]
    pub log: Option<Log>,
}

// Diagnostic detail, independent of step narration
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum Log {
    // Update and connect events up to debug level
    Debug,
    // Update, connect and wire events at every level
    Trace,
}

// Top-level command palette, shared by parsing, help and shell completion
#[derive(Subcommand)]
pub(crate) enum Command {
    /// List hardware Arks and running emulators
    Devices,
    /// Show the Ark's serial, firmware, pairing and lock
    Status(Recovery),
    /// Enroll the Ark at Ark Hub, or install an attestation
    Enroll(Enroll),
    /// Verify the Ark against Dark Bio's device registry
    Genuine,
    /// Pair the Ark with Ark Companion on your phone
    Pair,
    /// Unlock the Ark, approved on your phone
    Unlock,
    /// Read and change the datasets on the Ark
    #[command(subcommand)]
    Data(Data),
    /// Run an app, approved on your phone; print its report
    Run {
        /// WebAssembly app to run
        file: PathBuf,
    },
    /// List and install Ark firmware
    #[command(subcommand)]
    Firmware(Firmware),
    /// Check this computer, the Ark and the cloud; suggest fixes
    Doctor,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
    /// Print help for a command or a topic
    Help {
        /// Command, such as data upload, or topic name
        #[arg(num_args = 0.., value_name = "COMMAND_OR_TOPIC")]
        path: Vec<String>,
        /// Print the whole manual, every page and topic
        #[arg(long, conflicts_with = "path")]
        all: bool,
    },
}

// Explicit identity pin accepted by diagnostics and enrollment recovery
#[derive(Args)]
pub(crate) struct Recovery {
    /// Trust this public key instead of the attestation
    #[arg(long, value_name = "HEX", help_heading = "Advanced")]
    pub pubkey: Option<String>,
}

// Local attestation input and optional identity pin for enrollment
#[derive(Args)]
pub(crate) struct Enroll {
    /// Install the attestation in this file
    #[arg(long, value_name = "FILE")]
    pub cwt: Option<PathBuf>,
    // Recovery can authenticate a device whose stored attestation is unusable
    #[command(flatten)]
    pub recovery: Recovery,
}

// Dataset inspection and mutation commands; slot names map to wire IDs below
#[derive(Subcommand)]
pub(crate) enum Data {
    /// Map the data paths an app can read
    Paths,
    /// List the slots and their state
    List,
    /// Show one slot in full
    Show {
        /// Slot name or id from `ark data list`
        #[arg(value_parser = parse_slot)]
        slot: i32,
    },
    /// Upload the owner's variant calls, approved on your phone
    Upload {
        /// Variant call file (VCF) to upload
        file: PathBuf,
        /// Fail unless the Ark identifies this slot
        #[arg(long, value_parser = parse_slot)]
        slot: Option<i32>,
        /// Identify the file and plan, without changes
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
    },
    /// Download public data onto the Ark
    Fetch {
        /// Slot name or id from `ark data list`
        #[arg(value_parser = parse_slot, required_unless_present_any = ["all", "sample"], conflicts_with = "all")]
        slot: Option<i32>,
        /// Fetch every reference on offer, in order
        #[arg(long)]
        all: bool,
        /// Fill personal slots with a public genome, by name or ID
        #[arg(long, value_name = "NAME", value_parser = samples::Parser, hide_possible_values = true, conflicts_with = "all")]
        sample: Option<samples::Sample>,
        /// Show the plan without changing the Ark
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
        /// Download cache directory
        #[arg(long, value_name = "DIR", conflicts_with = "no_cache")]
        cache: Option<PathBuf>,
        /// Keep no copy in the cache
        #[arg(long)]
        no_cache: bool,
    },
    /// List the public genomes fetch can download
    Samples {
        /// Population code from `ark data samples`
        #[arg(value_parser = populations(), hide_possible_values = true, conflicts_with = "all")]
        population: Option<String>,
        /// Only male or female samples
        #[arg(long, value_enum, hide_possible_values = true)]
        sex: Option<Sex>,
        /// List every sample of every population
        #[arg(long)]
        all: bool,
    },
    /// Empty a filled slot, approved on your phone
    Delete(Change),
    /// Erase a slot in any state, approved on your phone
    Repair(Change),
}

// Shared selection and planning arguments for slot deletion and repair
#[derive(Args)]
pub(crate) struct Change {
    /// Slot name or id from `ark data list`
    #[arg(value_parser = parse_slot)]
    pub slot: i32,
    /// Show the plan without changing the Ark
    #[arg(long, conflicts_with = "unlock")]
    pub dry_run: bool,
}

// Published firmware selection and installation, including read-only planning
#[derive(Subcommand)]
pub(crate) enum Firmware {
    /// Show the installed build and the candidates
    List,
    /// Install firmware and restart the Ark
    Update {
        /// Version to install, as `ark firmware list` prints it; the newest
        /// candidate when omitted
        #[arg(value_name = "VERSION")]
        version: Option<String>,
        /// Show the plan without installing
        #[arg(long, conflicts_with = "unlock")]
        dry_run: bool,
        /// Exit once installed, without waiting for the reboot
        #[arg(long)]
        no_wait: bool,
        /// Confirm installing firmware and the reboot
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// Accepts a population code, and completes each one with its description.
fn populations() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(samples::POPULATIONS.iter().map(|population| {
        clap::builder::PossibleValue::new(population.code).help(population.description)
    }))
}

/// Parses a cloud environment name, one of `release`, `staging` or `develop`.
pub(crate) fn parse_env(value: &str) -> Result<Environment, String> {
    environments()
        .find(|environment| environment.to_string() == value)
        .ok_or_else(|| format!("unknown environment {value:?}; use release, staging or develop"))
}

/// Lists every environment, for the parser and its suggestions.
pub(crate) fn environments() -> impl Iterator<Item = Environment> {
    variants!(
        Environment,
        Environment::Release,
        Environment::Staging,
        Environment::Develop,
    )
    .into_iter()
}

/// Parses a slot selector, either a positive numeric id or an exact slot name.
///
/// A name must match the spelling [`slot_name`] prints. Any positive id is
/// accepted, so slots this build does not know stay addressable.
pub(crate) fn parse_slot(value: &str) -> Result<i32, String> {
    if let Ok(id) = value.parse::<i32>()
        && id > 0
    {
        return Ok(id);
    }
    let name = format!("SLOT_KIND_{}", value.replace('-', "_").to_ascii_uppercase());
    SlotKind::from_str_name(&name)
        .filter(|kind| *kind as i32 > 0 && slot_name(*kind as i32) == value)
        .map(i32::from)
        .ok_or_else(|| format!("unknown slot {value:?}; use a name or id from `ark data list`"))
}

/// Formats known protocol slot names, retaining unknown IDs as decimal selectors.
pub(crate) fn slot_name(id: i32) -> String {
    SlotKind::try_from(id)
        .map(|kind| {
            kind.as_str_name()
                .trim_start_matches("SLOT_KIND_")
                .to_ascii_lowercase()
                .replace('_', "-")
        })
        .unwrap_or_else(|_| id.to_string())
}

/// Lists the name of every slot kind this build knows, in id order, spelled as
/// [`parse_slot`] accepts it.
///
/// The wire's slot kinds are non-exhaustive, so no match can prove this list
/// complete, and a new kind is added here by hand.
pub(crate) fn slot_names() -> impl Iterator<Item = String> {
    [
        SlotKind::ReferenceGenome,
        SlotKind::GeneAnnotations,
        SlotKind::SmallVariants,
        SlotKind::VariantCatalog,
        SlotKind::StructuralVariants,
        SlotKind::CopyNumberVariants,
    ]
    .into_iter()
    .map(|kind| slot_name(kind as i32))
}

/// Parses a `--timeout` value in seconds, rejecting zero and values too large
/// for a deadline on the monotonic clock.
#[expect(
    clippy::disallowed_methods,
    reason = "flags are parsed before any connection clock exists, and the timeout must fit a deadline on the real monotonic clock that connections run on"
)]
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

/// Tests of the value parsers and the conflict checks across command levels.
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that `--sample` parses without a slot, and that naming any slot
    /// with it fails with a usage error hinting at the slotless form.
    #[test]
    fn test_sample_fetch_takes_no_slot() {
        for name in ["HG00096", "notable-owen"] {
            let cli = Cli::try_parse_from(["ark", "data", "fetch", "--sample", name]).unwrap();
            cli.validate().unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Data(Data::Fetch {
                    slot: None,
                    sample: Some(_),
                    ..
                }))
            ));

            for slot in [
                "small-variants",
                "structural-variants",
                "copy-number-variants",
                "1",
                "42",
            ] {
                let cli =
                    Cli::try_parse_from(["ark", "data", "fetch", slot, "--sample", name]).unwrap();
                let error = cli.validate().unwrap_err();
                assert_eq!((error.class, error.code), (2, "usage"), "{slot}");
                assert_eq!(error.message, "--sample takes no slot", "{slot}");
                assert_eq!(
                    error.hints,
                    ["run `ark data fetch --sample notable-owen`"],
                    "{slot}"
                );
            }
        }
    }

    /// Checks that every environment name parses, alone and as a global flag,
    /// while an unknown name fails.
    #[test]
    fn all_environments_parse() {
        assert!(
            parse_env("prod")
                .unwrap_err()
                .starts_with("unknown environment")
        );

        // Each name parses directly and through the flag before a command
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

    /// Checks that slot names parse only in their exact spelling, while every
    /// positive id parses, known or not.
    #[test]
    fn slots_are_exact_and_future_ids_remain_addressable() {
        let known = [
            (1, "reference-genome"),
            (2, "gene-annotations"),
            (3, "small-variants"),
            (4, "variant-catalog"),
            (5, "structural-variants"),
            (6, "copy-number-variants"),
        ];
        assert_eq!(
            slot_names().collect::<Vec<_>>(),
            known.map(|(_, name)| name)
        );
        for (id, name) in known {
            assert_eq!(slot_name(id), name);
            assert_eq!(parse_slot(name), Ok(id));
            assert_eq!(parse_slot(&id.to_string()), Ok(id));
        }
        assert_eq!(parse_slot("2147483647"), Ok(i32::MAX));

        // Zero, negative, overflowing, unknown and misspelled selectors all fail
        for invalid in [
            "0",
            "unspecified",
            "-1",
            "SMALL-VARIANTS",
            "small_variants",
            "small",
            "snp-indel-calls",
            "2147483648",
        ] {
            assert!(parse_slot(invalid).is_err());
        }
    }

    /// Checks that `-v` and `--log` set independently, while a repeated `-v`
    /// or an unknown log level fails.
    #[test]
    fn narration_and_diagnostics_are_independent() {
        let cli = Cli::try_parse_from(["ark", "-v", "status", "--log", "debug"]).unwrap();
        assert!(cli.options.verbose);
        assert_eq!(cli.options.log, Some(Log::Debug));
        let cli = Cli::try_parse_from(["ark", "--log", "trace", "status"]).unwrap();
        assert!(!cli.options.verbose);
        assert_eq!(cli.options.log, Some(Log::Trace));

        // Verbosity has no levels, and logs have no info level
        for args in [
            vec!["ark", "-vv"],
            vec!["ark", "-vvv"],
            vec!["ark", "--log", "info"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    /// Checks that firmware confirmation is explicit and accepts both spellings.
    #[test]
    fn test_firmware_confirmation_accepts_both_spellings() {
        for flag in [None, Some("-y"), Some("--yes")] {
            let mut args = vec!["ark", "firmware", "update"];
            args.extend(flag);
            let cli = Cli::try_parse_from(args).unwrap();
            let Some(Command::Firmware(Firmware::Update { yes, .. })) = cli.command else {
                panic!("expected firmware update");
            };
            assert_eq!(yes, flag.is_some(), "{flag:?}");
        }
    }

    /// `ark run` takes the app file, with the shared options before or after
    /// it.
    #[test]
    fn test_run_accepts_the_app_file_and_shared_options() {
        for args in [
            vec!["ark", "--json", "--unlock", "run", "sample app.wasm"],
            vec!["ark", "run", "sample app.wasm", "--json", "--unlock"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            cli.validate().unwrap();
            assert!(cli.options.json, "{args:?}");
            assert!(cli.options.unlock, "{args:?}");
            let Some(Command::Run { file }) = cli.command else {
                panic!("expected run");
            };
            assert_eq!(file, PathBuf::from("sample app.wasm"), "{args:?}");
        }
    }

    /// Checks that conflicting flags fail in the parser or in validation,
    /// whichever command level they arrive at.
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
            vec!["ark", "firmware", "update", "--dry-run", "--unlock"],
            vec!["ark", "--quiet", "status", "-v"],
        ] {
            assert!(
                Cli::try_parse_from(&args).map_or(true, |cli| cli.validate().is_err()),
                "{args:?}"
            );
        }
    }
}
