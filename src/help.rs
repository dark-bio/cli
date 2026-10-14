// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Help pages, topics and the manual, generated from the commands this build
//! serves.
//!
//! A page has one shape on a terminal and in a pipe; only color differs.

use crate::{
    args::Cli,
    error::Error,
    output::Output,
    style::{self, Color, Role, Theme},
};
use clap::CommandFactory;

/// Options every command accepts, listed once on the root page and hidden on
/// every other page, since an agent reads the manual from the root.
const GLOBAL: [&str; 9] = [
    "device", "json", "timeout", "unlock", "no_input", "env", "quiet", "verbose", "log",
];

/// Help topics in the order the manual prints them.
const TOPICS: [&str; 7] = [
    "agents", "devices", "datasets", "apps", "firmware", "output", "errors",
];

/// Maximum line width of help pages and topics in terminal cells.
const WIDTH: usize = 80;

/// A command's paragraph and optional leaf contract, selected by its path.
struct Contract {
    /// Command words after `ark`, or `ark` itself for the root page.
    path: &'static str,
    /// Explanation between the one-line description and usage.
    paragraph: &'static str,
    /// Contract fields for a leaf command, absent on root and group pages.
    footer: Option<ContractFooter>,
}

/// A leaf command's prerequisites, outcome and examples.
struct ContractFooter {
    /// Conditions the command needs before it can work.
    requires: &'static str,
    /// Who approves the command and where.
    approval: &'static str,
    /// Expected duration and the waits the timeout bounds.
    time: &'static str,
    /// Every key in the JSON document and the fields added by flags.
    json: &'static str,
    /// Two invocations printed as bare commands.
    examples: &'static str,
}

/// Command paragraphs and contract fields in command-tree order.
const CONTRACTS: &[Contract] = &[
    Contract {
        path: "ark",
        paragraph: "ark talks to an Ark, plugged in over USB or emulated by Ark Emulator, and to \
            the Dark Bio cloud behind it. The owner approves access to their data on their \
            phone, in Ark Companion, and ark cannot approve for them.",
        footer: None,
    },
    Contract {
        path: "devices",
        paragraph: "Discovery connects to none of them, so the names and serials are what it \
            reports, unverified. `ark status` shows the serial an Ark proves when it \
            connects. With several Arks attached, the hint names a selector for each to \
            pass to --device.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "seconds",
            json: "devices, each with locator, kind, name, serial, image, environment, ready",
            examples: "ark devices\nark devices --json",
        }),
    },
    Contract {
        path: "status",
        paragraph: "It also shows how ark trusts the Ark and whether the Ark is in sync with the \
            cloud, and hints at the next step. It works offline, unpaired and locked. On \
            firmware older than ark supports, it prints what it can read, then fails with \
            firmware-outdated.",
        footer: Some(ContractFooter {
            requires: "one Ark, in any state; the cloud is not needed",
            approval: "none",
            time: "seconds",
            json: "name, serial, hardware (version, revision, model), firmware (version, \
                published), environment, trust, synced, paired, unlocked, mismatch, realm, \
                identity, pubkey",
            examples: "ark status\nark --device emulator status --json",
        }),
    },
    Contract {
        path: "genuine",
        paragraph: "It syncs the Ark with the cloud first, then asks the registry whether the Ark \
            is enrolled and active. It fails with registry-inactive when the registration \
            is disabled, expired or superseded.",
        footer: Some(ContractFooter {
            requires: "one Ark and the cloud",
            approval: "none",
            time: "seconds",
            json: "serial, enrolled, active, disabled, expired, superseded",
            examples: "ark genuine\nark genuine --json",
        }),
    },
    Contract {
        path: "pair",
        paragraph: "Pairing happens once. ark prints a pairing link, with a QR code where the \
            locale and width allow, and under --json in the approve event. The owner opens \
            it in Ark Companion. The Ark and the phone then show colors, and the owner \
            presses the Ark's button with its pin when they match. The button sits behind \
            a pinhole under the Ark's bottom right LED.",
        footer: Some(ContractFooter {
            requires: "an unpaired Ark and the cloud",
            approval: "the owner opens the link in Ark Companion, then presses the Ark's button \
                within 60s if its colors match the phone's",
            time: "up to 10 minutes for the link and 60s for the button, then the storage setup",
            json: "serial, paired",
            examples: "ark pair\nark pair --json",
        }),
    },
    Contract {
        path: "unlock",
        paragraph: "The Ark then stays unlocked until it loses power. An Ark that is unlocked \
            already stays as it is, with a note.",
        footer: Some(ContractFooter {
            requires: "a paired Ark and the cloud",
            approval: "on your phone, unless the Ark is unlocked already",
            time: "up to 2 minutes for the approval",
            json: "unlocked, changed",
            examples: "ark unlock\nark unlock --json",
        }),
    },
    Contract {
        path: "enroll",
        paragraph: "Enrolling gives a self-signed Ark, such as a fresh emulator, an attested \
            identity. Without --cwt ark prints the Ark Hub address where the person \
            enrolls the Ark in a browser, then fails with enrollment-required. With --cwt \
            it installs the attestation from the file, then reconnects to check it.",
        footer: Some(ContractFooter {
            requires: "a self-signed Ark",
            approval: "none; the person enrolls the Ark in a browser at Ark Hub",
            time: "seconds",
            json: "enrolled, url; --cwt replaces url with name, serial, hardware (version, \
                revision, model), firmware (version, published), environment, trust, synced, \
                paired, unlocked, mismatch, realm, identity, pubkey; a failed reconnect \
                returns enrolled and status (null)",
            examples: "ark enroll\nark enroll --cwt attestation.cwt --json",
        }),
    },
    Contract {
        path: "data",
        paragraph: "An Ark keeps its data in slots, one per kind of dataset. These commands \
            describe the slots and the paths apps read, upload the owner's calls, download \
            public data, list public genomes and empty slots. All but samples need a \
            paired, unlocked Ark, and `ark help datasets` covers them all.",
        footer: None,
    },
    Contract {
        path: "data paths",
        paragraph: "The tree marks directories, grantable paths and missing data, with examples \
            beside them. --json describes each path in full, including how the phone words \
            a grant of it. The map never shows a value from the owner's data.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            json: "paths, each with path, directory, grantable, available, description, \
                format, examples, public, wording",
            examples: "ark data paths\nark data paths --json",
        }),
    },
    Contract {
        path: "data list",
        paragraph: "Each row shows a slot's state, build, version and size, and a hint names the \
            command that fills or erases a slot. `ark data show` prints one slot in full, \
            with its origin and requirements.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            json: "slots, each with slot, id, name, description, format, state, origin, \
                damage, requires, size_bytes, build, version, download (url, size_bytes, sha256)",
            examples: "ark data list\nark data list --json",
        }),
    },
    Contract {
        path: "data show",
        paragraph: "It shows the slot's state and dependencies beside the Ark's description of the \
            data and of the file that fills it. It also lists the slots that require it, \
            the download it offers and whether this computer's cache holds that download.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            json: "slot, id, name, description, format, state, origin, damage, requires, \
                size_bytes, build, version, download (url, size_bytes, sha256), required_by, cached",
            examples: "ark data show snp-indel-calls\nark data show 3 --json",
        }),
    },
    Contract {
        path: "data upload",
        paragraph: "The Ark identifies the file from its first bytes and accepts one person's \
            variant calls in VCF, plain or compressed, into the empty snp-indel-calls \
            slot. The owner approves on their phone, then ark sends the file and waits \
            while the Ark validates and indexes it. --dry-run stops after the \
            identification.",
        footer: Some(ContractFooter {
            requires: "an empty target slot, a paired, unlocked Ark and the cloud (--unlock \
                only if status reports locked)",
            approval: "on your phone within 1 minute; none with --dry-run",
            time: "minutes to an hour; each processing step reports its own progress",
            json: "slot, id, confidence, uploaded_bytes, phases, duration_seconds; \
                --dry-run adds state and requires",
            examples: "ark data upload calls.vcf.gz\nark data upload calls.vcf.gz --dry-run --json",
        }),
    },
    Contract {
        path: "data fetch",
        paragraph: "ark streams the file the Ark offers for a reference slot, checking it \
            against the offered SHA-256 on the way. --sample instead fills the \
            snp-indel-calls slot with a genome from `ark data samples`. The whole file \
            downloads and is checked against the size and SHA-256 built into ark before \
            its upload asks for approval, and that upload is never retried. A copy stays \
            in this computer's cache. A filled slot is skipped, with no download and no \
            approval. --all fetches every reference slot that offers a file, in the \
            order their requirements set, and skips the rest.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark; the cloud unless status reports synced, and \
                always for a --sample upload's approval; the download host to download a file \
                not yet cached; for a reference, an offered download (--unlock only if status \
                reports locked)",
            approval: "on your phone within 1 minute once a --sample download is \
                verified; none for a reference; --unlock needs your phone",
            time: "minutes per slot, an hour or more for the variant catalog; network failures \
                get 3 attempts",
            json: "fetched, each with slot, id, url, size_bytes, sha256, cached, outcome, \
                error; --sample adds sample and name; --dry-run marks pending rows as planned",
            examples: "ark data fetch --all\n\
                ark data fetch snp-indel-calls --sample quick-prairie --dry-run --json",
        }),
    },
    Contract {
        path: "data samples",
        paragraph: "The genomes are the variant calls of the 1000 Genomes Project's 3,202 \
            people, made by Illumina's DRAGEN 4.4.7. Each keeps a fixed name made from \
            its sample ID, such as quick-prairie for HG02345, and `ark data fetch \
            --sample` takes either. With no population the list shows the 26 \
            populations, and with one it shows that population's people. --all shows \
            everyone instead of a population, and --sex narrows either to one sex.",
        footer: Some(ContractFooter {
            requires: "nothing, since the list is built into ark",
            approval: "none",
            time: "instant",
            json: "populations, each with population, count, description; with a population \
                or --all, samples, each with name, sample, population, sex, father, mother",
            examples: "ark data samples JPT\nark data samples JPT --sex female --json",
        }),
    },
    Contract {
        path: "data delete",
        paragraph: "An empty slot is refused, and so is a damaged or half-written one, which `ark \
            data repair` erases. --dry-run shows the slot and the slots that require it, \
            and changes nothing.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark and the cloud (--unlock only if status reports \
                locked)",
            approval: "on your phone within 2 minutes; none with --dry-run",
            time: "seconds after the approval",
            json: "slot, id, state, changed; --dry-run adds required_by",
            examples: "ark data delete snp-indel-calls\nark data delete snp-indel-calls --dry-run --json",
        }),
    },
    Contract {
        path: "data repair",
        paragraph: "It recovers a damaged or half-written slot that delete refuses, and it erases \
            a healthy dataset too. --dry-run shows the slot and the slots that require it, \
            and changes nothing.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark and the cloud (--unlock only if status reports \
                locked)",
            approval: "on your phone within 2 minutes; none with --dry-run",
            time: "seconds after the approval",
            json: "slot, id, state, changed; --dry-run adds required_by",
            examples: "ark data repair snp-indel-calls\nark data repair snp-indel-calls --dry-run --json",
        }),
    },
    Contract {
        path: "run",
        paragraph: "ark sends the app, and the Ark checks it before the owner approves the run, \
            switching on optional grants and answering the app's questions. Once the app \
            ends, the owner reviews its report. ark prints it if they release it, and \
            fails if they decline. When the app reports failure, ark still prints the \
            released report, then fails with app-failed.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark and the cloud (--unlock only if status reports \
                locked)",
            approval: "on your phone twice, the run within 5 minutes and its report within 10 \
                minutes",
            time: "the two approvals, plus however long the app runs; --timeout bounds only \
                machine replies",
            json: "task, app (name, version, \
                develop), success, paths, media, stdout or stdout_base64, stderr or \
                stderr_base64, duration_seconds",
            examples: "ark run app.wasm > report.md\nark run app.wasm --json",
        }),
    },
    Contract {
        path: "firmware",
        paragraph: "An Ark runs ArkOS, its firmware. These commands list the builds published \
            for the Ark and install one. An emulated Ark runs the firmware bundled with Ark \
            Emulator instead.",
        footer: None,
    },
    Contract {
        path: "firmware list",
        paragraph: "The builds list newest first, for the Ark's cloud environment, with each \
            candidate's headline whole below the table. `ark help firmware` explains which \
            builds are candidates.",
        footer: Some(ContractFooter {
            requires: "one Ark and its package host",
            approval: "none",
            time: "seconds",
            json: "installed, update, firmwares (version, published, size_bytes, sha256, summary, installed, \
                candidate)",
            examples: "ark firmware list\nark firmware list --json",
        }),
    },
    Contract {
        path: "firmware update",
        paragraph: "It installs the newest candidate, or the version given, then waits for the Ark \
            to come back running it. It needs confirmation, at a terminal or through \
            --yes, and the Ark's state decides who approves.",
        footer: Some(ContractFooter {
            requires: "one Ark, the cloud and its package host",
            approval: "none if unpaired; the Ark's button within 30s if locked; your phone \
                within 2 minutes if unlocked; --yes confirms the installation here",
            time: "minutes, then up to 120s for the Ark to return; --no-wait skips the return",
            json: "from, to, size_bytes, approval, installed, returned, verified, \
                running",
            examples: "ark firmware update --dry-run\nark firmware update 0.12.0-74d557e --yes --json",
        }),
    },
    Contract {
        path: "doctor",
        paragraph: "Runs every check in order and prints the whole report, this computer first, \
            then the Ark. Each check is ok, warn, fail or skip, with a hint where something \
            needs action, and a summary closes the report. It applies no fixes, though its \
            checks sync the Ark with the cloud and look up the newest ark release, which \
            local builds and CI skip. The first failed check sets the exit code, which a \
            warning never does.",
        footer: Some(ContractFooter {
            requires: "nothing; a check that cannot run is skipped",
            approval: "none",
            time: "seconds per check",
            json: "version, checks, each with name, result (ok, warn, fail or skip), detail and hint",
            examples: "ark doctor\nark doctor --json",
        }),
    },
    Contract {
        path: "completions",
        paragraph: "ark prints the script on stdout and never installs it.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "immediate",
            json: "none; the completion script stays text even with --json",
            examples: "ark completions zsh\nark completions bash > ark.bash",
        }),
    },
    Contract {
        path: "help",
        paragraph: "A topic is agents, devices, datasets, apps, firmware, output or errors. A name \
            that is both a topic and a command, devices or firmware, prints the topic, and \
            `ark devices --help` prints the command's page. `ark help --all` prints the \
            whole manual, starting with the root page and the agents topic.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "immediate",
            json: "none; help stays text even with --json",
            examples: "ark help agents\nark help --all",
        }),
    },
];

/// Resolves the stdout theme for help, which is written for reading even when
/// the invocation selects JSON.
///
/// The theme never follows that flag, but a pipe still loses color.
pub(crate) fn theme() -> Theme {
    Theme::new(false, false)
}

/// Builds the executable command tree with shared styling and command-specific
/// contracts.
pub(crate) fn command(theme: &Theme) -> clap::Command {
    let mut command = Cli::command();
    decorate(&mut command, "", theme);
    command.build();
    compact(&mut command, theme, true);
    command
}

/// Lays out every page in clap's short help layout, wrapped to the width, and
/// hides the shared options on every page but the root.
///
/// Clap normally expands long help onto two lines per option. The short layout
/// is rendered once and used for every help spelling.
fn compact(command: &mut clap::Command, theme: &Theme, root: bool) {
    // Hide shared options only in the display copy, since mut_arg moves
    // arguments and invalidates the parser's indexes after build
    let mut display = command
        .clone()
        .before_help(None)
        .before_long_help(None)
        .after_help(None)
        .after_long_help(None);
    if !root {
        for name in GLOBAL {
            display = display.mut_arg(name, |argument| argument.hide(true));
        }
    }

    // Render the scan alone, placing blank lines between the root's groups
    let scan = display
        .render_help()
        .ansi()
        .to_string()
        .lines()
        .map(|line| {
            let plain = console::strip_ansi_codes(line);
            let gap = root && (plain.starts_with("  data ") || plain.starts_with("  firmware "));
            format!(
                "{}{}",
                if gap { "\n" } else { "" },
                style::wrap(&theme.inline(line), theme.width.min(WIDTH), hanging(line))
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (description, scan) = scan.split_once("\n\n").unwrap_or((&scan, ""));
    *command = command.clone().help_template(format!(
        "{description}\n\n{{before-help}}{}{{after-help}}",
        scan.trim_end()
    ));

    // Subcommands get the same layout, without the shared options
    for child in command.get_subcommands_mut() {
        compact(child, theme, false);
    }
}

/// Finds the description column of a rendered row, ignoring its ANSI styles.
///
/// A row separates its name and description with at least two spaces. Other
/// lines continue at their own indent.
fn hanging(line: &str) -> usize {
    let plain = console::strip_ansi_codes(line);
    let body = plain.trim_start();
    let indent = plain.len() - body.len();
    let column = match body.find("  ") {
        Some(gap) => {
            let rest = &body[gap..];
            indent + gap + (rest.len() - rest.trim_start().len())
        }
        None => indent,
    };
    console::measure_text_width(&plain[..column])
}

/// Adds each command's paragraph, its contract footer and its examples.
///
/// The command path selects its contract; clap still owns syntax and argument help.
fn decorate(command: &mut clap::Command, parent: &str, theme: &Theme) {
    // Style the page, and give each subcommand back the help flag that the
    // root disables for its whole tree
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
                .help("Print help"),
        );
    }

    // The command path, without the root's name, selects the contract
    let path = if parent.is_empty() {
        command.get_name().to_string()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let key = path.strip_prefix("ark ").unwrap_or(&path);
    let contract = CONTRACTS
        .iter()
        .find(|contract| contract.path == key)
        .unwrap_or_else(|| panic!("missing help contract for {key}"));

    // The root gets the overview, a group points to its children, and a leaf
    // gets its contract
    let help = if parent.is_empty() {
        format!(
            "{}\n\n{}",
            exit_codes(theme),
            theme.inline(&format!(
                "Output is formatted for reading; --json keeps complete, exact values.
AI agents: read `ark help agents` first.
Topics: {}.",
                TOPICS.join(", ")
            ))
        )
    } else if let Some(fields) = &contract.footer {
        footer(
            theme,
            &[
                ("Requires", fields.requires),
                ("Approval", fields.approval),
                ("Time", fields.time),
                ("JSON", fields.json),
            ],
            fields.examples,
        )
    } else {
        theme.inline(&format!(
            "Each subcommand has its own requirements, approvals and output.\nRead `ark {key} COMMAND --help` for its contract."
        ))
    };

    // Attach the paragraph and the contract, which every help spelling prints
    let help = help
        .lines()
        .map(|line| style::wrap(line, theme.width.min(WIDTH), hanging(line)))
        .collect::<Vec<_>>()
        .join("\n");
    let paragraph = style::wrap(&theme.inline(contract.paragraph), theme.width.min(WIDTH), 0);
    *command = command
        .clone()
        .before_long_help(paragraph.clone())
        .after_long_help(format!("{help}\n"))
        .before_help(paragraph)
        .after_help(format!("{help}\n"));

    // Subcommands get their own contracts under this path
    for child in command.get_subcommands_mut() {
        decorate(child, &path, theme);
    }
}

/// Prints a command page, an embedded topic or the full manual without discovery.
///
/// Help remains readable text even when the invocation selects JSON.
pub(crate) fn run(output: &Output, path: &[String], all: bool) -> Result<(), Error> {
    let theme = theme();
    let mut root = command(&theme);

    // The manual is the root page, the agents topic, every command page, then
    // the other topics, divided by a muted line
    if all {
        let mut pages = Vec::new();
        collect_help(&mut root, &mut pages);
        pages.insert(1, markdown(&theme, topic(TOPICS[0]).unwrap()));
        pages.extend(
            TOPICS[1..]
                .iter()
                .map(|name| markdown(&theme, topic(name).unwrap())),
        );
        return output.text(&pages.join(&format!(
            "\n\n{}\n\n",
            theme.paint(Role::Muted, "-".repeat(theme.width.min(WIDTH)))
        )));
    }

    // A single name that is a topic prints the topic, even when a command
    // shares it
    if path.len() == 1
        && let Some(topic) = topic(&path[0])
    {
        return output.text(&markdown(&theme, topic));
    }

    // Anything else walks the command tree to the page it names
    let mut command = &mut root;
    for (index, name) in path.iter().enumerate() {
        if command.find_subcommand(name).is_none() {
            // Let clap suggest a child command, then try topics at the root
            let parsed = command
                .clone()
                .try_get_matches_from([command.get_name(), name]);
            let suggestions = parsed
                .err()
                .map(|error| {
                    crate::usage::suggestions(&error, clap::error::ContextKind::SuggestedSubcommand)
                })
                .unwrap_or_default();
            if index == 0 && suggestions.is_empty() {
                return Err(unknown_topic(name));
            }
            let mut error = Error::new(2, "usage", format!("unknown help command '{name}'"));
            let parent = std::iter::once("ark")
                .chain(path[..index].iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            for suggested in suggestions {
                error = error.hint(format!("a similar command exists: `{parent} {suggested}`"));
            }
            if error.hints.is_empty() {
                error = error.hint(format!("see `{parent} --help`"));
            }
            return Err(error);
        }
        command = command.find_subcommand_mut(name).unwrap();
    }
    output.text(&command.render_long_help().ansi().to_string())
}

/// Builds the error for an unknown topic, with a near match or the help page as
/// its hint.
fn unknown_topic(name: &str) -> Error {
    let mut error = Error::new(2, "usage", format!("unknown help topic '{name}'"));
    if let Some(suggested) = crate::usage::similar(name, TOPICS) {
        error = error.hint(format!("a similar topic exists: `ark help {suggested}`"));
    } else {
        error = error.hint("see `ark help --help`");
    }
    error
}

/// Lays out the contract with its labels padded to one column and its values
/// wrapped under themselves.
///
/// The examples follow as bare commands, since a pasted prompt breaks in a
/// shell.
fn footer(theme: &Theme, fields: &[(&str, &str)], examples: &str) -> String {
    let column = fields
        .iter()
        .map(|(label, _)| label.len() + 4)
        .max()
        .unwrap_or(0);
    let mut lines = vec![theme.paint(Role::Heading, "Contract:")];
    lines.extend(fields.iter().map(|(label, text)| {
        style::wrap(
            &format!(
                "  {}{}{}",
                theme.paint(Role::Muted, label),
                " ".repeat(column - label.len() - 2),
                theme.inline(text)
            ),
            theme.width.min(WIDTH),
            column,
        )
    }));

    // The examples follow under their own heading, styled as commands
    lines.push(format!("\n{}", theme.paint(Role::Heading, "Examples:")));
    lines.extend(examples.lines().map(|line| {
        style::wrap(
            &format!("  {}", theme.paint(Role::Accent, line)),
            theme.width.min(WIDTH),
            2,
        )
    }));
    lines.join("\n")
}

/// Renders the exit codes in two columns, or in one on a narrow terminal.
fn exit_codes(theme: &Theme) -> String {
    let entries = [
        ("0", "done"),
        ("1", "a local input or a missing confirmation"),
        ("2", "usage"),
        ("3", "reaching the Ark"),
        ("4", "the cloud"),
        ("5", "the Ark's state or refusal"),
        ("6", "an approval that did not come"),
        ("7", "a machine wait that ran out"),
        ("8", "an app failure"),
        ("130", "Ctrl-C"),
        ("143", "SIGTERM"),
    ];
    let mut lines = vec![theme.paint(Role::Heading, "Exit codes:")];
    if theme.width >= WIDTH {
        for (index, (code, meaning)) in entries[..6].iter().enumerate() {
            let mut row = format!("  {}  {meaning}", theme.paint(Role::Muted, code));
            if let Some((right, meaning)) = entries.get(index + 6) {
                row.push_str(&" ".repeat(45 - console::measure_text_width(&row)));
                row.push_str(&format!(
                    "{}  {meaning}",
                    theme.paint(Role::Muted, format!("{right:>3}"))
                ));
            }
            lines.push(row);
        }
    } else {
        for (code, meaning) in entries {
            lines.push(style::wrap(
                &format!(
                    "  {}  {meaning}",
                    theme.paint(Role::Muted, format!("{code:>3}"))
                ),
                theme.width,
                7,
            ));
        }
    }
    lines.join("\n")
}

/// Renders a help topic's Markdown dialect for reading.
///
/// The dialect has headings, bullets with their continuation lines, and code
/// that is either fenced or indented by four spaces or more. Indented code
/// drops the block's own indent so long commands stay on one line.
fn markdown(theme: &Theme, text: &str) -> String {
    let mut fenced = false;
    let mut bullet = false;
    let mut block = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        // Fences toggle a code block and print nothing themselves
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }

        // Style the line by its kind, keeping the hanging indent its wrap needs
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
        lines.push(style::wrap(&line, theme.width.min(WIDTH), hanging));
    }
    lines.join("\n").trim_end().to_string()
}

/// Collects command pages in command-tree order for the complete manual.
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

/// Returns a compiled-in help topic by its public name.
fn topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "agents" => include_str!("help/agents.md"),
        "devices" => include_str!("help/devices.md"),
        "datasets" => include_str!("help/datasets.md"),
        "apps" => include_str!("help/apps.md"),
        "firmware" => include_str!("help/firmware.md"),
        "output" => include_str!("help/output.md"),
        "errors" => include_str!("help/errors.md"),
        _ => return None,
    })
}

/// Tests of help pages, contract footers and topic rendering.
#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

    /// Every global option keeps its meaning at every level of the parsed tree.
    #[test]
    fn test_decorated_command_parses_global_options_at_every_level() {
        // Check the whole option set so a flag cannot silently set another one
        for (flags, key, expected) in [
            (
                vec!["--device", "hardware"],
                "device",
                serde_json::json!("hardware"),
            ),
            (
                vec!["-d", "emulator"],
                "device",
                serde_json::json!("emulator"),
            ),
            (vec!["--json"], "json", serde_json::json!(true)),
            (vec!["--unlock"], "unlock", serde_json::json!(true)),
            (vec!["--no-input"], "no_input", serde_json::json!(true)),
            (vec!["--timeout", "120"], "timeout", serde_json::json!(120)),
            (
                vec!["--env", "staging"],
                "env",
                serde_json::json!("staging"),
            ),
            (vec!["--quiet"], "quiet", serde_json::json!(true)),
            (vec!["-q"], "quiet", serde_json::json!(true)),
            (vec!["--verbose"], "verbose", serde_json::json!(true)),
            (vec!["-v"], "verbose", serde_json::json!(true)),
            (vec!["--log", "trace"], "log", serde_json::json!("Trace")),
        ] {
            for path in [vec!["status"], vec!["data", "list"]] {
                for level in 0..=path.len() {
                    let mut arguments = vec!["ark"];
                    arguments.extend_from_slice(&path[..level]);
                    arguments.extend_from_slice(&flags);
                    arguments.extend_from_slice(&path[level..]);
                    let matches = command(&Theme::test(80, Color::Off, true))
                        .try_get_matches_from(&arguments)
                        .unwrap_or_else(|error| panic!("{arguments:?}: {error}"));
                    let options = Cli::from_arg_matches(&matches).unwrap().options;
                    let actual = serde_json::json!({
                        "device":options.device, "json":options.json,
                        "unlock":options.unlock, "no_input":options.no_input,
                        "timeout":options.timeout, "env":options.env.map(|env| env.to_string()),
                        "quiet":options.quiet, "verbose":options.verbose,
                        "log":options.log.map(|log| format!("{log:?}")),
                    });
                    let mut wanted = serde_json::json!({
                        "device":null, "json":false, "unlock":false, "no_input":false,
                        "timeout":60, "env":null, "quiet":false, "verbose":false, "log":null,
                    });
                    wanted[key] = expected.clone();
                    assert_eq!(actual, wanted, "{arguments:?}");
                }
            }
        }
    }

    /// `--no-input` never turns into `--unlock`, wherever it stands.
    #[test]
    fn test_decorated_no_input_never_unlocks() {
        for arguments in [
            vec!["ark", "--no-input", "data", "list"],
            vec!["ark", "data", "--no-input", "list"],
            vec!["ark", "data", "list", "--no-input"],
        ] {
            let matches = command(&Theme::test(80, Color::Off, true))
                .try_get_matches_from(&arguments)
                .unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert!(cli.options.no_input, "{arguments:?}");
            assert!(!cli.options.unlock, "{arguments:?}");
        }
    }

    /// Checks that every help spelling prints the paragraph after the one-liner.
    #[test]
    fn test_paragraphs_follow_descriptions_on_every_command() {
        /// Checks both layouts before walking the command's children.
        fn visit(command: &mut clap::Command) {
            let long = command.render_long_help().ansi().to_string();
            let long = console::strip_ansi_codes(&long);
            let (opening, _) = long.split_once("\n\nUsage:").unwrap();
            let (description, paragraph) = opening.split_once("\n\n").unwrap();
            assert!(!paragraph.trim().is_empty(), "{}", command.get_name());
            assert_eq!(paragraph, paragraph.trim(), "{}", command.get_name());
            assert_eq!(
                description.split_whitespace().collect::<Vec<_>>().join(" "),
                command.get_about().unwrap().to_string(),
                "{}",
                command.get_name()
            );

            let short = command.render_help().ansi().to_string();
            let short = console::strip_ansi_codes(&short);
            assert_eq!(short, long, "{}", command.get_name());
            for child in command.get_subcommands_mut() {
                visit(child);
            }
        }

        // Both color modes and widths keep the same paragraph boundaries
        for width in [60, 80] {
            for color in [Color::Off, Color::True] {
                let theme = Theme::test(width, color, true);
                visit(&mut command(&theme));
            }
        }
    }

    /// Checks that option, argument and advanced rows wrap under their
    /// descriptions, including when the row carries color.
    #[test]
    fn test_wrapped_rows_keep_their_description_columns() {
        /// Checks every continuation of a row against its first description.
        fn check(page: &str, name: &str, expected_column: usize) {
            let plain = console::strip_ansi_codes(page);
            let mut lines = plain.lines().skip_while(|line| !line.contains(name));
            let first = lines.next().unwrap();
            assert!(!first[expected_column..].trim().is_empty(), "{first}");
            let continuations: Vec<_> = lines
                .take_while(|line| line.starts_with(&" ".repeat(expected_column)))
                .collect();
            assert!(!continuations.is_empty(), "{name}: {plain}");
            for line in continuations {
                assert_eq!(
                    line.len() - line.trim_start().len(),
                    expected_column,
                    "{name}: {line}"
                );
            }
            assert!(
                plain
                    .lines()
                    .all(|line| console::measure_text_width(line) <= 60)
            );
        }

        // Real root and recovery rows exercise both clap and the advanced block
        for color in [Color::Off, Color::True] {
            let theme = Theme::test(60, color, true);
            let mut root = command(&theme);
            check(&root.render_help().ansi().to_string(), "--device", 27);
            for name in ["status", "enroll"] {
                let page = root
                    .find_subcommand_mut(name)
                    .unwrap()
                    .render_long_help()
                    .ansi()
                    .to_string();
                check(&page, "--pubkey", 22);
            }

            // A contract value wraps under its own start
            let upload = root
                .find_subcommand_mut("data")
                .unwrap()
                .find_subcommand_mut("upload")
                .unwrap()
                .render_help()
                .ansi()
                .to_string();
            check(&upload, "  Requires  ", 12);
            check(&upload, "  JSON      ", 12);

            // A positional row uses a shorter name column than root options
            let mut sample = clap::Command::new("sample")
                .about("Inspect a file")
                .styles(theme.clap())
                .color(if color == Color::Off {
                    clap::ColorChoice::Never
                } else {
                    clap::ColorChoice::Always
                })
                .arg(clap::Arg::new("FILE").help(
                    "A local file whose description continues beyond the first line at this width",
                ));
            sample.build();
            compact(&mut sample, &theme, true);
            check(&sample.render_help().ansi().to_string(), "[FILE]  ", 10);
        }
    }

    /// The footer aligns its labels and styles its examples, and a topic
    /// styles its headings, code and bullets.
    #[test]
    fn footer_uses_aligned_labels_and_styled_examples() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            footer(
                &theme,
                &[("Requires", "a paired Ark"), ("Approval", "only if locked")],
                "ark unlock"
            ),
            "\x1b[1mContract:\x1b[0m\n  Requires  a paired Ark\n  Approval  only if locked\n\n\x1b[1mExamples:\x1b[0m\n  \x1b[1mark unlock\x1b[0m"
        );
        assert_eq!(
            markdown(
                &theme,
                "# States\n\nUse `ark status`.\n\n- Keep the phone nearby.\n\n```sh\nark unlock\n```\n"
            ),
            "\x1b[1mStates\x1b[0m\n\nUse `\x1b[1mark status\x1b[0m`.\n\n  - Keep the phone nearby.\n\n  \x1b[1mark unlock\x1b[0m"
        );
    }

    /// Checks heading, label and exit-number colors on the rendered pages.
    #[test]
    fn test_contract_labels_and_exit_numbers_are_muted() {
        let theme = Theme::test(80, Color::True, true);
        let mut root = command(&theme);
        let page = root
            .find_subcommand_mut("status")
            .unwrap()
            .render_help()
            .ansi()
            .to_string();
        assert!(page.contains(&theme.paint(Role::Heading, "Contract:")));
        for label in ["Requires", "Approval", "Time", "JSON"] {
            assert!(page.contains(&theme.paint(Role::Muted, label)), "{label}");
        }
        let page = root.render_help().ansi().to_string();
        assert!(page.contains(&theme.paint(Role::Heading, "Exit codes:")));
        for code in [
            "0", "1", "2", "3", "4", "5", "  6", "  7", "  8", "130", "143",
        ] {
            assert!(page.contains(&theme.paint(Role::Muted, code)), "{code}");
        }
    }

    /// A terminal and a pipe print the same page; only color differs.
    #[test]
    fn pages_have_one_shape_with_and_without_color() {
        let colored = Theme::test(80, Color::True, true);
        let plain = Theme {
            interactive: false,
            unicode: false,
            color: Color::Off,
            ..colored.clone()
        };
        let mut styled = command(&colored);
        let mut bare = command(&plain);
        for path in [vec!["data", "upload"], vec!["status"], vec![]] {
            let (mut styled, mut bare) = (&mut styled, &mut bare);
            for name in &path {
                styled = styled.find_subcommand_mut(name).unwrap();
                bare = bare.find_subcommand_mut(name).unwrap();
            }
            let rendered = styled.render_long_help().ansi().to_string();
            assert!(rendered.contains("\x1b["), "{path:?}");
            assert_eq!(
                console::strip_ansi_codes(&rendered),
                bare.render_long_help().to_string(),
                "{path:?}"
            );
        }
    }

    /// Help pages and topic prose wrap at 80 columns on a wide terminal.
    #[test]
    fn test_help_wraps_at_eighty_columns_on_wide_terminals() {
        for color in [Color::Off, Color::True] {
            // Every command page keeps its prose within the reading width
            let theme = Theme::test(200, color, true);
            let mut pages = Vec::new();
            collect_help(&mut command(&theme), &mut pages);
            for page in pages {
                assert!(
                    page.lines()
                        .all(|line| console::measure_text_width(line) <= 80),
                    "{color:?}: {page}"
                );
            }

            // A topic wraps long paragraphs and bullets under their own text
            let prose = "Read `ark status` before starting a task. Keep the phone nearby to approve access to the data on the Ark.";
            let rendered = markdown(&theme, &format!("{prose}\n\n- {prose}"));
            assert_eq!(
                console::strip_ansi_codes(&rendered),
                "Read `ark status` before starting a task. Keep the phone nearby to approve\naccess to the data on the Ark.\n\n  - Read `ark status` before starting a task. Keep the phone nearby to approve\n    access to the data on the Ark.",
                "{color:?}"
            );
        }
    }

    /// Command pages keep backticks around inline commands with one style span.
    #[test]
    fn test_help_keeps_backticks() {
        for (color, expected) in [
            (Color::Off, "`ark help agents`"),
            (Color::Basic, "`\x1b[1mark help agents\x1b[0m`"),
        ] {
            let theme = Theme::test(80, color, true);
            let page = command(&theme).render_long_help().ansi().to_string();
            assert!(page.contains(expected), "{color:?}");
        }
    }

    /// Help fits a narrow terminal and keeps its example commands whole.
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
        assert!(console::strip_ansi_codes(&rendered).contains("ark data fetch --all"));
        assert!(rendered.contains("\x1b["));
    }

    /// Shared options are listed on the root page and hidden on child pages,
    /// which keep their help flag.
    #[test]
    fn shared_options_are_listed_on_the_root_page_only() {
        let theme = Theme::test(80, Color::Off, false);
        let mut root = command(&theme);

        // The root page lists the shared options. Row indents and value names
        // distinguish the listings from prose and examples that mention them.
        let listed = [
            "--timeout <SECONDS>",
            "      --json ",
            "      --no-input ",
            "      --log <LEVEL>",
            "-d, --device <SELECTOR>",
        ];
        let page = root.render_help().to_string();
        for option in listed {
            assert!(page.contains(option), "{option}");
        }

        // Child pages hide them, and keep their help flag
        for path in [vec!["data"], vec!["data", "upload"], vec!["doctor"]] {
            let mut command = &mut root;
            for name in &path {
                command = command.find_subcommand_mut(name).unwrap();
            }
            let page = command.render_long_help().to_string();
            for option in listed {
                assert!(!page.contains(option), "{path:?}: {option}");
            }
            assert!(page.contains("-h, --help"), "{path:?}");
        }
    }

    /// Group pages point to their children's contracts, and leaf pages carry
    /// their own.
    #[test]
    fn groups_point_to_child_contracts() {
        let theme = Theme::test(80, Color::Off, false);
        let mut root = command(&theme);

        // Group pages point to their children without a contract of their own
        for name in ["data", "firmware"] {
            let group = root.find_subcommand_mut(name).unwrap();
            let long = group.render_long_help().to_string();
            assert!(long.contains("Each subcommand has its own requirements"));
            assert!(!long.contains("Contract:"));
        }

        // Leaf pages carry their contracts, down to the unlock guidance
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
            let long = long.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(long.contains("--unlock only if status reports locked"));
            assert!(long.contains("none; --unlock needs your phone"));
        }
    }

    /// Topic bullets align their continuation lines, and indented code drops
    /// its block indent.
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
