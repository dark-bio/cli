// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Help is generated from the commands this build actually serves.

use crate::{
    args::{Cli, Format},
    error::Error,
    style::{self, Color, Role, Theme},
};
use clap::CommandFactory;

/// Builds the executable command tree with shared styling and command-specific contracts.
pub(crate) fn command(theme: &Theme) -> clap::Command {
    let mut command = Cli::command();
    decorate(&mut command, "", theme);
    command.build();
    compact(&mut command, true, theme);
    command
}

/// Clap normally expands long help onto two lines per option. Render its short
/// layout once, then let the help action select the short or long footer.
fn compact(command: &mut clap::Command, root: bool, theme: &Theme) {
    let mut display = command.clone().after_help(None).after_long_help(None);
    if !root {
        for arg in command.get_arguments().filter(|arg| arg.is_global_set()) {
            display = display.mut_arg(arg.get_id().clone(), |arg| arg.hide(true));
        }
    }
    let rendered = display.render_help();
    let scan = if theme.human {
        rendered
            .ansi()
            .to_string()
            .lines()
            .map(|line| style::wrap(&theme.inline(line), theme.width, 6))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        rendered.to_string()
    };
    *command = command
        .clone()
        .help_template(format!("{}{{after-help}}", scan.trim_end()));
    for child in command.get_subcommands_mut() {
        compact(child, false, theme);
    }
}

/// Adds prerequisites, approval guidance, output fields and examples to each command.
/// The command path selects its contract; clap still owns syntax and argument help.
fn decorate(command: &mut clap::Command, parent: &str, theme: &Theme) {
    *command = command
        .clone()
        .styles(theme.clap())
        .color(if theme.color == Color::Off {
            clap::ColorChoice::Never
        } else {
            clap::ColorChoice::Always
        });
    if !parent.is_empty() {
        *command = command.clone().arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("Print help (see more with '--help')")
                .long_help("Print help (see a summary with '-h')"),
        );
    }
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
            "devices: locator, kind, name, serial, image, environment, ready; the last two need a connection",
            "ark devices\nark devices --format json",
        ),
        "status" => (
            "one Ark (works offline, including while unpaired or locked)",
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
            "enrolled, url for online enrollment; enrolled and status fields with --cwt",
            "ark enroll\nark enroll --cwt attestation.cwt",
        ),
        "data list" => (
            "a paired, unlocked Ark (pass --unlock if it is locked)",
            "none; --unlock needs your phone",
            "seconds",
            "slots: slot, id, name, description, state, origin, requires, size_bytes, build, version, damage, download",
            "ark data list\nark data list --unlock --format json",
        ),
        "data show" => (
            "a paired, unlocked Ark (pass --unlock if it is locked)",
            "none; --unlock needs your phone",
            "seconds",
            "slot, id, name, description, state, origin, requires, size_bytes, build, version, damage, download, required_by, cached",
            "ark data show snp-indel-calls\nark data show 3 --format json",
        ),
        "data paths" => (
            "a paired, unlocked Ark (pass --unlock if it is locked)",
            "none; --unlock needs your phone",
            "seconds",
            "the Ark's dataset README verbatim; JSON: readme",
            "ark data paths\nark data paths --unlock --format json",
        ),
        "data upload" => (
            "a local file and a paired, unlocked Ark (pass --unlock if it is locked)",
            "on your phone for personal data; none for reference data or --dry-run",
            "minutes to an hour; each processing step has its own ETA",
            "slot, id, confidence, uploaded_bytes, phases, duration_seconds; state, requires under --dry-run",
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
            "a paired, unlocked Ark (pass --unlock if it is locked)",
            "on your phone; never under --dry-run",
            "up to a minute for approval",
            "slot, id, state, changed; required_by under --dry-run",
            "ark data delete snp-indel-calls --dry-run\nark data repair snp-indel-calls",
        ),
        "app run" => (
            "a local WASM file and a paired, unlocked Ark (pass --unlock if it is locked)",
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
            "installed, update, firmwares: version, published, size_bytes, sha256, summary, installed, candidate",
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
            "checks: name, result, detail, hint; tool, connect, wire, minimum_firmware, minimum_develop_publish",
            "ark doctor\nark doctor --format json",
        ),
        "completions" => (
            "nothing",
            "none",
            "immediate",
            "shell completion text in every format",
            "ark completions bash\nark completions zsh",
        ),
        _ => (
            "nothing",
            "none",
            "immediate",
            "help text in every format",
            "ark --help\nark help agents",
        ),
    };
    let exits = match key {
        "devices" => "0 done; 1 local; 2 usage; 3 device",
        "status" => {
            "0 done; 1 local; 2 usage; 3 device; 5 Ark (including outdated firmware, not pairing or lock state); 7 timeout"
        }
        "enroll" => "0 done; 1 local; 2 usage; 3 device; 5 Ark; 7 timeout",
        "genuine" | "app cancel" | "firmware list" | "doctor" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 7 timeout"
        }
        "pair" | "unlock" | "data list" | "data show" | "data paths" | "data upload"
        | "data fetch" | "data delete" | "data repair" | "firmware update" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout"
        }
        "app run" => {
            "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout; 8 app"
        }
        _ => "0 done; 1 local; 2 usage",
    };
    let help = format!(
        "Requires: {requires}\nApproval: {approval}\nTime:     {time}\nPrints:   {prints}\nExit:     {exits}; 130/143 interrupted\n\nExamples:\n  {}\n\nGlobal options: ark --help",
        examples.replace('\n', "\n  ")
    );
    let help = if theme.human {
        footer(
            theme,
            &[
                ("Requires", requires),
                ("Approval", approval),
                ("Time", time),
                ("Prints", prints),
                ("Exit", &format!("{exits}; 130/143 interrupted")),
            ],
            examples,
        )
    } else {
        help
    };
    let help = if parent.is_empty() {
        "Output defaults to human on terminals, text in pipes (per stream).
Scripts and AI agents: read `ark help agents` first.
Topics: agents, states, output, devices, datasets, apps."
            .to_string()
    } else if command.get_subcommands().next().is_some() {
        format!(
            "Each subcommand has its own requirements, approvals and output.\nRead `ark {key} COMMAND --help` for its contract."
        )
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
    let help = if theme.human {
        help.lines()
            .map(|line| style::wrap(&theme.inline(line), theme.width, 0))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        help
    };
    let mut decorated = command.clone().after_long_help(format!("{help}\n"));
    if parent.is_empty() {
        decorated = decorated.after_help(format!("{help}\n"));
    }
    *command = decorated;
    for child in command.get_subcommands_mut() {
        decorate(child, &path, theme);
    }
}

/// Prints a command page, an embedded topic or the full manual without discovery.
/// Help remains readable text even when the invocation selects JSON.
pub(crate) fn run(path: &[String], all: bool, format: Format) -> Result<(), Error> {
    let theme = Theme::new(format, false);
    let mut root = command(&theme);
    if all {
        if theme.human {
            let mut pages = Vec::new();
            collect_help(&mut root, &mut pages);
            pages.extend(
                ["agents", "states", "output", "devices", "datasets", "apps"]
                    .map(|name| markdown(&theme, topic(name).unwrap())),
            );
            println!(
                "{}",
                pages.join(&format!(
                    "\n\n{}\n\n",
                    theme.paint(Role::Muted, "-".repeat(theme.width.min(80)))
                ))
            );
            return Ok(());
        }
        print_command(&mut root)?;
        for name in ["agents", "states", "output", "devices", "datasets", "apps"] {
            println!("\n{}", topic(name).unwrap().trim_end());
        }
        return Ok(());
    }
    if path.len() == 1
        && let Some(topic) = topic(&path[0])
    {
        println!(
            "{}",
            if theme.human {
                markdown(&theme, topic)
            } else {
                topic.trim_end().to_string()
            }
        );
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

/// Aligns short contract labels and shell examples within the human terminal width.
fn footer(theme: &Theme, fields: &[(&str, &str)], examples: &str) -> String {
    let mut lines = fields
        .iter()
        .map(|(label, text)| {
            style::wrap(
                &format!(
                    "{}{}{}",
                    theme.paint(Role::Muted, label),
                    " ".repeat(11 - label.len()),
                    theme.inline(text)
                ),
                theme.width,
                11,
            )
        })
        .collect::<Vec<_>>();
    lines.push(format!("\n{}", theme.paint(Role::Heading, "Examples")));
    lines.extend(examples.lines().map(|line| {
        style::wrap(
            &format!("  $ {}", theme.paint(Role::Accent, line)),
            theme.width,
            4,
        )
    }));
    lines.push(format!(
        "\n{} {}",
        theme.paint(Role::Muted, "Global options:"),
        theme.paint(Role::Accent, "ark --help")
    ));
    lines.join("\n")
}

/// Renders the topic dialect: headings, bullets with their continuation lines,
/// and code that is either fenced or indented by four spaces or more. Indented
/// code drops the block's own indent so long commands stay on one line.
fn markdown(theme: &Theme, text: &str) -> String {
    let mut fenced = false;
    let mut bullet = false;
    let mut block = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let content = line.trim_start();
        let indent = line.len() - content.len();
        let (line, hanging) = if fenced {
            (format!("  {}", theme.paint(Role::Accent, line)), 2)
        } else if indent >= 4 && !content.is_empty() {
            let base = *block.get_or_insert(indent);
            let code = &line[base.min(indent)..];
            let pad = if bullet { 4 } else { 2 };
            (
                format!("{}{}", " ".repeat(pad), theme.paint(Role::Accent, code)),
                pad,
            )
        } else {
            block = None;
            if content.is_empty() {
                (String::new(), 0)
            } else if line.starts_with('#') {
                bullet = false;
                let heading = content.trim_start_matches('#').trim_start();
                (theme.paint(Role::Heading, heading), 0)
            } else if line.starts_with("- ") {
                bullet = true;
                (format!("  {}", theme.inline(line)), 4)
            } else if bullet && indent > 0 {
                (format!("    {}", theme.inline(content)), 4)
            } else {
                bullet = false;
                (theme.inline(line), 0)
            }
        };
        lines.push(style::wrap(&line, theme.width, hanging));
    }
    lines.join("\n").trim_end().to_string()
}

/// Collects human command pages in command-tree order for the complete manual.
fn collect_help(command: &mut clap::Command, pages: &mut Vec<String>) {
    pages.push(
        command
            .render_long_help()
            .ansi()
            .to_string()
            .trim_end()
            .to_string(),
    );
    for child in command.get_subcommands_mut() {
        collect_help(child, pages);
    }
}
/// Prints this command and its descendants as plain long-help pages.
fn print_command(command: &mut clap::Command) -> Result<(), Error> {
    command.print_long_help()?;
    println!("\n");
    for child in command.get_subcommands_mut() {
        print_command(child)?;
    }
    Ok(())
}
/// Returns a compiled-in help topic by its public name.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_uses_aligned_labels_and_styled_examples() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            footer(
                &theme,
                &[("Requires", "a paired Ark"), ("Approval", "only if locked")],
                "ark unlock"
            ),
            "Requires   a paired Ark\nApproval   only if locked\n\n\x1b[1mExamples\x1b[0m\n  $ \x1b[1mark unlock\x1b[0m\n\nGlobal options: \x1b[1mark --help\x1b[0m"
        );
        assert_eq!(
            markdown(
                &theme,
                "# States\n\nUse `ark status`.\n\n- Keep the phone nearby.\n\n```sh\nark unlock\n```\n"
            ),
            "\x1b[1mStates\x1b[0m\n\nUse \x1b[1mark status\x1b[0m.\n\n  - Keep the phone nearby.\n\n  \x1b[1mark unlock\x1b[0m"
        );
    }

    #[test]
    fn help_fits_narrow_terminals_without_losing_commands() {
        let theme = Theme::test(60, Color::True, true);
        let mut command = command(&theme);
        let fetch = command
            .find_subcommand_mut("data")
            .unwrap()
            .find_subcommand_mut("fetch")
            .unwrap();
        let rendered = fetch.render_long_help().ansi().to_string();
        assert!(
            rendered
                .lines()
                .all(|line| console::measure_text_width(line) <= theme.width)
        );
        assert!(console::strip_ansi_codes(&rendered).contains("ark data fetch --all --unlock"));
        assert!(rendered.contains("\x1b["));
    }

    #[test]
    fn groups_point_to_child_contracts() {
        let theme = Theme::test(80, Color::Off, false);
        let mut root = command(&theme);
        for name in ["data", "app", "firmware"] {
            let group = root.find_subcommand_mut(name).unwrap();
            let long = group.render_long_help().to_string();
            assert!(long.contains("Each subcommand has its own requirements"));
            assert!(!long.contains("Requires:"));
            assert!(!long.contains("Approval:"));
            assert!(!long.contains("Exit:"));
        }
        let status = root.find_subcommand_mut("status").unwrap();
        assert!(
            status
                .render_long_help()
                .to_string()
                .contains("works offline")
        );
        let data = root.find_subcommand_mut("data").unwrap();
        for name in ["list", "show", "paths"] {
            let long = data
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(long.contains("pass --unlock if it is locked"));
            assert!(long.contains("none; --unlock needs your phone"));
        }
    }

    #[test]
    fn markdown_aligns_bullets_and_dedents_indented_code() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            markdown(
                &theme,
                "- First line of a bullet\n  continues here.\n\n      ark status\n\n  Back in the bullet.\nPlain again.\n"
            ),
            "  - First line of a bullet\n    continues here.\n\n    \x1b[1mark status\x1b[0m\n\n    Back in the bullet.\nPlain again."
        );
    }
}
