// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Help is generated from the commands this build actually serves.

use crate::{args::Cli, error::Error};
use clap::CommandFactory;

pub(crate) fn command() -> clap::Command {
    let mut command = Cli::command();
    decorate(&mut command, "");
    command.build();
    compact(&mut command, true);
    command
}

/// Clap normally expands long help onto two lines per option. Render its short
/// layout once, then let the help action select the short or long footer.
fn compact(command: &mut clap::Command, root: bool) {
    let mut display = command.clone().after_help(None).after_long_help(None);
    if !root {
        for arg in command.get_arguments().filter(|arg| arg.is_global_set()) {
            display = display.mut_arg(arg.get_id().clone(), |arg| arg.hide(true));
        }
    }
    let scan = display.render_help().to_string();
    *command = command
        .clone()
        .help_template(format!("{}{{after-help}}", scan.trim_end()));
    for child in command.get_subcommands_mut() {
        compact(child, false);
    }
}

fn decorate(command: &mut clap::Command, parent: &str) {
    let path = if parent.is_empty() {
        command.get_name().to_string()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let key = path.strip_prefix("ark ").unwrap_or(&path);
    let (requires, approval, time, prints, examples) = match key {
        "devices" => (
            "nothing",
            "none",
            "seconds",
            "devices: locator, kind, name, serial, image, environment, ready",
            "ark devices\nark devices --format json",
        ),
        "status" => (
            "one Ark; no cloud access",
            "none",
            "seconds",
            "name, serial, hardware, firmware, trust, environment, realm, synced, paired, unlocked, identity, pubkey, mismatch",
            "ark status\nark -d emulator status --format json",
        ),
        "genuine" => (
            "one Ark and a cloud environment",
            "none",
            "seconds",
            "serial, enrolled, active, disabled, expired, superseded",
            "ark genuine\nark genuine --env develop --format json",
        ),
        "pair" => (
            "an unpaired Ark and cloud access",
            "scan in Ark Companion and confirm colours",
            "up to 10 minutes to scan; then approval and storage setup",
            "serial, paired; pairing URL on stderr",
            "ark pair\nark pair --format json",
        ),
        "unlock" => (
            "a paired Ark and cloud access",
            "on your phone unless already unlocked",
            "up to a minute for approval; unlocking lasts until power is cut",
            "unlocked, changed",
            "ark unlock\nark unlock --format json",
        ),
        "enroll" => (
            "one Ark; --cwt accepts an existing attestation",
            "online enrollment uses the Hub; none with --cwt",
            "seconds for --cwt and reconnection",
            "enrolled and status, or the online enrollment URL",
            "ark enroll\nark enroll --cwt attestation.cwt",
        ),
        "data list" => (
            "a paired, unlocked Ark (add --unlock)",
            "none",
            "seconds",
            "slots: slot, id, name, description, state, origin, requires, size_bytes, build, version, damage, download",
            "ark data list\nark data list --unlock --format json",
        ),
        "data show" => (
            "a paired, unlocked Ark (add --unlock)",
            "none",
            "seconds",
            "slot metadata, required_by, cached",
            "ark data show snp-indel-calls\nark data show 3 --format json",
        ),
        "data paths" => (
            "a paired, unlocked Ark (add --unlock)",
            "none",
            "seconds",
            "the Ark's dataset README verbatim; JSON: readme",
            "ark data paths\nark data paths --unlock --format json",
        ),
        "data upload" => (
            "a local file and a paired, unlocked Ark (add --unlock)",
            "on your phone for personal data; none for reference data or --dry-run",
            "minutes to an hour; each processing step has its own ETA",
            "slot, id, confidence, uploaded_bytes, phases, duration_seconds",
            "ark data upload calls.vcf.gz\nark data upload calls.vcf.gz --dry-run",
        ),
        "data fetch" => (
            "a paired, unlocked Ark and an advertised download",
            "none for reference downloads; --unlock may require your phone",
            "a large catalog may take an hour; network stalls retry up to 3 attempts",
            "fetched: slot, id, url, size_bytes, sha256, cached, outcome, error",
            "ark data fetch --all --unlock\nark data fetch reference-genome --dry-run --format json",
        ),
        "data delete" | "data repair" => (
            "a paired, unlocked Ark (add --unlock)",
            "on your phone; never under --dry-run",
            "up to a minute for approval",
            "slot, id, state, changed; required_by under --dry-run",
            "ark data delete snp-indel-calls --dry-run\nark data repair snp-indel-calls",
        ),
        "app run" => (
            "a local WASM file and a paired, unlocked Ark (add --unlock)",
            "on your phone before running",
            "unbounded run; --timeout bounds replies, not the whole app",
            "exact report bytes; JSON: task, app, success, stdout or stdout_base64, stderr or stderr_base64, duration_seconds",
            "ark app run app.wasm > report.md\nark app run app.wasm --unlock --format json",
        ),
        "app cancel" => (
            "one Ark and a task id from app run",
            "none",
            "seconds",
            "task, cancelled",
            "ark app cancel 42\nark app cancel 18446744073709551615 --format json",
        ),
        "firmware list" => (
            "one Ark and its package host",
            "none; develop and staging package hosts may need browser login",
            "seconds",
            "installed, update, firmwares",
            "ark firmware list\nark firmware list --format json",
        ),
        "firmware update" => (
            "one Ark, cloud and package access",
            "none if unpaired; button if locked; phone if unlocked; --yes confirms reboot",
            "minutes plus up to 120 s for reboot verification; --no-wait skips that wait",
            "from, to, size_bytes, approval, installed, returned, verified, running",
            "ark firmware update --dry-run\nark firmware update --yes --format json",
        ),
        "doctor" => (
            "nothing; unavailable checks are skipped",
            "none; develop and staging package hosts may need browser login",
            "seconds per check",
            "checks: name, result, detail, hint; tool, connect, wire",
            "ark doctor\nark doctor --format json",
        ),
        _ => (
            "nothing",
            "none",
            "immediate",
            "help or shell completion text",
            "ark --help\nark help agents",
        ),
    };
    let exits = match key {
        "devices" => "0 done; 1 local; 2 usage; 3 device",
        "status" | "enroll" => "0 done; 1 local; 2 usage; 3 device; 5 Ark; 7 timeout",
        "genuine" | "app cancel" | "firmware list" | "doctor" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 7 timeout"
        }
        "pair" | "unlock" | "data" | "data list" | "data show" | "data paths" | "data upload"
        | "data fetch" | "data delete" | "data repair" | "firmware" | "firmware update" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout"
        }
        "app" | "app run" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout; 8 app"
        }
        _ => "0 done; 1 local; 2 usage",
    };
    let help = format!(
        "Requires: {requires}\nApproval: {approval}\nTime:     {time}\nPrints:   {prints}\nExit:     {exits}; 130/143 interrupted\n\nExamples:\n  {}\n\nGlobal options: ark --help",
        examples.replace('\n', "\n  ")
    );
    let help = if parent.is_empty() {
        "Scripts and AI agents: use --format json for structured results.
Read `ark help agents` first. Topics: agents, states, output, devices, datasets, apps."
            .to_string()
    } else {
        let advanced = if matches!(key, "status" | "enroll") {
            "Advanced:
      --pubkey <HEX>  Pin an xDSA public key instead of verifying the attestation

"
        } else {
            ""
        };
        format!("{advanced}{help}")
    };
    *command = command.clone().after_long_help(format!("{help}\n"));
    for child in command.get_subcommands_mut() {
        decorate(child, &path);
    }
}

pub(crate) fn run(path: &[String], all: bool) -> Result<(), Error> {
    let mut root = command();
    if all {
        print_command(&mut root)?;
        for name in ["agents", "states", "output", "devices", "datasets", "apps"] {
            println!("\n{}", topic(name).unwrap());
        }
        return Ok(());
    }
    if path.len() == 1
        && let Some(topic) = topic(&path[0])
    {
        println!("{topic}");
        return Ok(());
    }
    let mut command = &mut root;
    for name in path {
        command = command.find_subcommand_mut(name).ok_or_else(|| {
            Error::new(
                2,
                "usage",
                format!("unknown help command or topic {name:?}"),
            )
        })?;
    }
    command.print_long_help()?;
    Ok(())
}
fn print_command(command: &mut clap::Command) -> Result<(), Error> {
    command.print_long_help()?;
    println!("\n");
    for child in command.get_subcommands_mut() {
        print_command(child)?;
    }
    Ok(())
}
fn topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "agents" => include_str!("help/agents.md"),
        "states" => include_str!("help/states.md"),
        "output" => include_str!("help/output.md"),
        "devices" => include_str!("help/devices.md"),
        "datasets" => include_str!("help/datasets.md"),
        "apps" => include_str!("help/apps.md"),
        _ => return None,
    })
}
