// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Help pages, topics and the manual, generated from the commands this build
//! serves.
//!
//! A page has one shape on a terminal and in a pipe; only color and glyphs
//! differ.

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
const TOPICS: [&str; 6] = [
    "agents", "devices", "datasets", "apps", "firmware", "output",
];

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
    /// Result fields in reading order and any JSON differences.
    prints: &'static str,
    /// Exit classes the command can return, before the shared interrupts.
    exit: &'static str,
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
        paragraph: "Lists hardware Arks on USB and the emulators their launchers report, without \
            connecting to any. Names and serials are what discovery reports, and only a \
            connection verifies them. With several Arks attached, --device takes a locator \
            from this list.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "seconds",
            prints: "locator, name, serial, kind, environment, ready; JSON adds image",
            exit: "0 done; 1 local; 2 usage; 3 device",
            examples: "ark devices\nark devices --json",
        }),
    },
    Contract {
        path: "status",
        paragraph: "Connects to the Ark and shows what it is, how ark trusts it, its firmware, \
            and its cloud, pairing and lock state. It works offline, unpaired and locked, and \
            hints at the next step. On firmware older than ark supports, it prints what it \
            can read, then fails with firmware-outdated.",
        footer: Some(ContractFooter {
            requires: "one Ark, in any state; the cloud is not needed",
            approval: "none",
            time: "seconds",
            prints: "name, serial, hardware, firmware, trust, environment, realm, synced, \
                paired, unlocked, identity, mismatch; JSON adds pubkey",
            exit: "0 done; 1 local; 2 usage; 3 device; 5 outdated firmware; 7 timeout",
            examples: "ark status\nark --device emulator status --json",
        }),
    },
    Contract {
        path: "genuine",
        paragraph: "Syncs the Ark with the cloud, then asks Dark Bio's device registry whether \
            the Ark is enrolled and active. It fails with registry-inactive when the \
            registration is disabled, expired or superseded.",
        footer: Some(ContractFooter {
            requires: "one Ark and the cloud",
            approval: "none",
            time: "seconds",
            prints: "serial, enrolled, active, disabled, expired, superseded",
            exit: "0 active; 1 local; 2 usage; 3 device; 4 cloud or an inactive registration; \
                5 Ark; 7 timeout",
            examples: "ark genuine\nark genuine --json",
        }),
    },
    Contract {
        path: "pair",
        paragraph: "Pairs an unpaired Ark with the owner's phone, once. ark prints a pairing \
            link, drawn as a QR code at a terminal and carried in an approve event elsewhere. \
            The owner opens it in Ark Companion within 10 minutes. The Ark and the phone then \
            show colors, and the owner presses the Ark's button with its pin within 60s when \
            they match. The button sits behind a pinhole under the Ark's bottom right LED.",
        footer: Some(ContractFooter {
            requires: "an unpaired Ark and the cloud",
            approval: "the owner opens the link in Ark Companion, then presses the Ark's button \
                within 60s if its colors match the phone's",
            time: "up to 10 minutes for the link and 60s for the button, then the storage setup",
            prints: "serial, paired; the pairing link on stderr",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark pair\nark pair --json",
        }),
    },
    Contract {
        path: "unlock",
        paragraph: "Asks the owner's phone to unlock the Ark, and waits up to 2 minutes for the \
            approval. The Ark then stays unlocked until it loses power. An Ark that is \
            unlocked already stays as it is, with a note.",
        footer: Some(ContractFooter {
            requires: "a paired Ark and the cloud",
            approval: "on your phone, unless the Ark is unlocked already",
            time: "up to 2 minutes for the approval; the Ark stays unlocked until it loses power",
            prints: "unlocked, changed",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark unlock\nark unlock --json",
        }),
    },
    Contract {
        path: "enroll",
        paragraph: "Gives a self-signed Ark, such as a fresh emulator, an attested identity. \
            Without --cwt it prints the Ark Hub address where the person enrolls the Ark in a \
            browser, then fails with enrollment-required. With --cwt it installs the \
            attestation from the file, then reconnects to check it.",
        footer: Some(ContractFooter {
            requires: "a self-signed Ark; with --cwt, an attestation file",
            approval: "none; the person enrolls the Ark in a browser at Ark Hub",
            time: "seconds",
            prints: "enrolled, url; with --cwt, enrolled and the status fields",
            exit: "0 installed with --cwt; 1 local or enrollment-required; 2 usage; 3 device; \
                4 cloud; 5 Ark; 7 timeout",
            examples: "ark enroll\nark enroll --cwt attestation.cwt",
        }),
    },
    Contract {
        path: "data",
        paragraph: "An Ark keeps its data in slots, one per kind of dataset. These commands \
            describe the slots and the paths apps read, upload the owner's calls, download \
            public references and empty slots. Each needs a paired, unlocked Ark, and \
            `ark help datasets` covers them all.",
        footer: None,
    },
    Contract {
        path: "data paths",
        paragraph: "Prints every path an app can read on this Ark as a tree, marking \
            directories, grantable paths and missing data, with examples beside them. --json \
            describes each path in full, including how the phone words a grant of it. The \
            map never shows a value from the owner's data.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            prints: "the path tree with its marks and examples; JSON paths: path, directory, \
                grantable, available, description, format, examples, public, wording",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data paths\nark data paths --json",
        }),
    },
    Contract {
        path: "data list",
        paragraph: "Lists every slot with its state, origin, build, version, size and the slots \
            it requires, and hints at the command that fills or erases a slot. \
            `ark data show` prints one slot in full.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            prints: "slot, state, origin, build, version, size (JSON size_bytes), requires; \
                JSON adds id, name, description, format, damage, download",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data list\nark data list --json",
        }),
    },
    Contract {
        path: "data show",
        paragraph: "Prints one slot in full. Beside the slot's state and dependencies, it shows \
            the Ark's description of the data and of the file that fills it, the slots that \
            require it, the download it offers and whether this computer's cache holds that \
            download.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark, and the cloud unless status reports synced \
                (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "seconds",
            prints: "slot, id, name, description, format, state, origin, damage, dependencies \
                (JSON requires), size (size_bytes), build, version, download, required by \
                (required_by), cached",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data show snp-indel-calls\nark data show 3 --json",
        }),
    },
    Contract {
        path: "data upload",
        paragraph: "Uploads the owner's variant calls, a VCF file with one person's genotypes, \
            into the empty snp-indel-calls slot. The Ark identifies the file from its first \
            bytes and refuses any other kind. The owner then approves on their phone within \
            1 minute, and ark uploads the file and waits while the Ark validates and indexes \
            it. --dry-run stops after the identification.",
        footer: Some(ContractFooter {
            requires: "a VCF file, an empty target slot, a paired, unlocked Ark and the cloud \
                (--unlock only if status reports locked)",
            approval: "on your phone within 1 minute; none with --dry-run",
            time: "minutes to an hour; each processing step reports its own progress",
            prints: "slot, id, confidence, uploaded (JSON uploaded_bytes), phases, duration \
                (duration_seconds); --dry-run adds state and dependencies (requires)",
            exit: "0 done; 1 local or an unidentified file; 2 usage; 3 device; 4 cloud; 5 Ark \
                or a failed validation; 6 approval; 7 timeout",
            examples: "ark data upload calls.vcf.gz\nark data upload calls.vcf.gz --dry-run",
        }),
    },
    Contract {
        path: "data fetch",
        paragraph: "Downloads the public reference file the Ark offers for a slot that is not \
            filled, and streams it to the Ark, checking it against the offered SHA-256 on the \
            way. A copy stays in this computer's cache. --all fetches every reference slot \
            that is not filled and offers a file, in the order their requirements set, and \
            skips the rest. A reference needs no approval.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark offering a download, the download host, and the \
                cloud unless status reports synced (--unlock only if status reports locked)",
            approval: "none; --unlock needs your phone",
            time: "minutes per slot, an hour or more for the variant catalog; network failures \
                get 3 attempts",
            prints: "slot, size (JSON size_bytes), cached, outcome; JSON fetched rows add id, \
                url, sha256, error",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data fetch --all\nark data fetch --all --dry-run --json",
        }),
    },
    Contract {
        path: "data delete",
        paragraph: "Empties a slot that holds a dataset, once the owner approves on their phone \
            within 2 minutes, and refuses an empty slot. --dry-run shows the slot and the \
            slots that require it, and changes nothing.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark and the cloud (--unlock only if status reports \
                locked)",
            approval: "on your phone within 2 minutes; none with --dry-run",
            time: "seconds after the approval",
            prints: "slot, id, state, changed; --dry-run adds required_by",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data delete snp-indel-calls\nark data delete snp-indel-calls --dry-run",
        }),
    },
    Contract {
        path: "data repair",
        paragraph: "Erases a slot whatever its state, once the owner approves on their phone \
            within 2 minutes. It recovers a damaged or half-written slot that delete refuses, \
            and it erases a healthy dataset too. --dry-run shows the slot and the slots that \
            require it, and changes nothing.",
        footer: Some(ContractFooter {
            requires: "a paired, unlocked Ark and the cloud (--unlock only if status reports \
                locked)",
            approval: "on your phone within 2 minutes; none with --dry-run",
            time: "seconds after the approval",
            prints: "slot, id, state, changed; --dry-run adds required_by",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 6 approval; 7 timeout",
            examples: "ark data repair snp-indel-calls\nark data repair snp-indel-calls --dry-run",
        }),
    },
    Contract {
        path: "app",
        paragraph: "An app is a WebAssembly module the Ark runs against the owner's data, once \
            the owner approves on their phone. `ark help apps` covers writing one and how a \
            run goes.",
        footer: None,
    },
    Contract {
        path: "app run",
        paragraph: "Uploads the app and follows its task. The Ark checks it, then the owner \
            approves the run on their phone within 5 minutes, switching on optional grants \
            and answering the app's questions. Once the app ends, the owner reviews its \
            report within 10 minutes. ark prints the report if they release it, and fails if \
            they decline.",
        footer: Some(ContractFooter {
            requires: "a WebAssembly file, a paired, unlocked Ark and the cloud (--unlock only \
                if status reports locked)",
            approval: "on your phone twice, the run within 5 minutes and its report within 10 \
                minutes",
            time: "the two approvals, plus however long the app runs; --timeout bounds only \
                machine replies",
            prints: "the report as the app printed it; JSON: task, app (name, version, \
                develop), success, paths, media, stdout or stdout_base64, stderr or \
                stderr_base64, duration_seconds",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark or a refused app; \
                6 approval; 7 timeout; 8 app failed",
            examples: "ark app run app.wasm > report.md\nark app run app.wasm --json",
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
        paragraph: "Lists the installed build and the published builds an update may install, \
            for the Ark's cloud environment. `ark help firmware` explains which builds are \
            candidates.",
        footer: Some(ContractFooter {
            requires: "one Ark and its package host",
            approval: "none",
            time: "seconds",
            prints: "version, published, size, flags, summary; JSON: installed, update, \
                firmwares (version, published, size_bytes, sha256, summary, installed, \
                candidate)",
            exit: "0 done; 1 local; 2 usage; 3 device; 4 cloud; 5 Ark; 7 timeout",
            examples: "ark firmware list\nark firmware list --json",
        }),
    },
    Contract {
        path: "firmware update",
        paragraph: "Installs the newest candidate, or the build --version names, then waits up \
            to 120s for the Ark to come back running it. It needs confirmation, at a \
            terminal or through --yes. The owner then approves nothing on an unpaired Ark, \
            presses the Ark's button with its pin within 30s on a locked one, and approves on \
            the phone within 2 minutes on an unlocked one.",
        footer: Some(ContractFooter {
            requires: "one Ark, the cloud and its package host",
            approval: "none if unpaired; the Ark's button within 30s if locked; your phone \
                within 2 minutes if unlocked; --yes confirms the installation here",
            time: "minutes, then up to 120s for the Ark to return; --no-wait skips the return",
            prints: "from, to, size (JSON size_bytes), approval, installed, returned, verified, \
                running",
            exit: "0 done; 1 local or unconfirmed; 2 usage; 3 device; 4 cloud; 5 Ark; \
                6 approval; 7 timeout",
            examples: "ark firmware update --dry-run\nark firmware update --yes --json",
        }),
    },
    Contract {
        path: "doctor",
        paragraph: "Runs every check in order and prints the whole list. It covers this tool \
            and its newer releases, discovery, the connection, the firmware, the cloud \
            environment and sync, the registry, pairing, the relay, the slots and the \
            download cache. Each check is ok, warn, fail or skip, with a hint where something \
            needs action. It applies no fixes, though its checks sync the Ark with the cloud \
            and look up the newest ark release.",
        footer: Some(ContractFooter {
            requires: "nothing; a check that cannot run is skipped",
            approval: "none",
            time: "seconds per check",
            prints: "each check's result (ok, warn, fail or skip), name, detail and hint; JSON: \
                tool, connect, wire, minimum_firmware, minimum_develop_publish, checks",
            exit: "0 without a failed check; otherwise the class of the first failure, \
                1 local, 3 device, 4 cloud, 5 Ark or 7 timeout",
            examples: "ark doctor\nark doctor --json",
        }),
    },
    Contract {
        path: "completions",
        paragraph: "Prints the completion script for the shell on stdout. It never installs \
            the script.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "immediate",
            prints: "the completion script, as text even with --json",
            exit: "0 done; 2 usage",
            examples: "ark completions zsh\nark completions bash",
        }),
    },
    Contract {
        path: "help",
        paragraph: "Prints the help page of a command, or a topic: agents, devices, datasets, \
            apps, firmware or output. `ark help --all` prints the whole manual, starting with \
            the root page and the agents topic.",
        footer: Some(ContractFooter {
            requires: "nothing",
            approval: "none",
            time: "immediate",
            prints: "help text, even with --json",
            exit: "0 done; 1 local; 2 unknown command or topic",
            examples: "ark help agents\nark help --all",
        }),
    },
];

/// Resolves the stdout theme for help, which is written for reading even when
/// the invocation selects JSON.
///
/// The theme never follows that flag, but a pipe still loses color and glyphs.
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
/// is rendered once, and the help action selects the paragraph and footer.
fn compact(command: &mut clap::Command, theme: &Theme, root: bool) {
    // Hide the shared options below the root
    if !root {
        for name in GLOBAL {
            *command = command
                .clone()
                .mut_arg(name, |argument| argument.hide(true));
        }
    }

    // Render the scan alone, then let the help action select the paragraph
    // after its description and the footer after its arguments and options
    let mut display = command
        .clone()
        .before_help(None)
        .before_long_help(None)
        .after_help(None)
        .after_long_help(None);
    let scan = display
        .render_help()
        .ansi()
        .to_string()
        .lines()
        .map(|line| style::wrap(&theme.inline(line), theme.width, hanging(line)))
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
                .help("Print help (see more with '--help')")
                .long_help("Print help (see a summary with '-h')"),
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
            "Output is formatted for reading; --json keeps complete, exact values.
AI agents: read `ark help agents` first.
Topics: {}.",
            TOPICS.join(", ")
        )
    } else if let Some(fields) = &contract.footer {
        // Clap keeps an option hidden from the short page out of the rendered
        // scan too, so the long page lists it by hand ahead of the contract
        let advanced = if matches!(key, "status" | "enroll") {
            "Advanced:
      --pubkey <HEX>  Trust this public key instead of the attestation

"
        } else {
            ""
        };
        format!(
            "{advanced}{}",
            footer(
                theme,
                &[
                    ("Requires", fields.requires),
                    ("Approval", fields.approval),
                    ("Time", fields.time),
                    ("Prints", fields.prints),
                    ("Exit", &format!("{}; 130/143 interrupted", fields.exit)),
                ],
                fields.examples,
            )
        )
    } else {
        format!(
            "Each subcommand has its own requirements, approvals and output.\nRead `ark {key} COMMAND --help` for its contract."
        )
    };

    // Wrap the text and attach it as the long footer, and on the root as the
    // short one too
    let help = help
        .lines()
        .map(|line| style::wrap(&theme.inline(line), theme.width, hanging(line)))
        .collect::<Vec<_>>()
        .join("\n");
    let paragraph = style::wrap(&theme.inline(contract.paragraph), theme.width, 0);
    let mut decorated = command
        .clone()
        .before_long_help(paragraph.clone())
        .after_long_help(format!("{help}\n"));
    if parent.is_empty() {
        decorated = decorated
            .before_help(paragraph)
            .after_help(format!("{help}\n"));
    }
    *command = decorated;

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
            theme.paint(Role::Muted, "-".repeat(theme.width.min(80)))
        )));
    }

    // A single name may be a topic
    if path.len() == 1
        && let Some(topic) = topic(&path[0])
    {
        return output.text(&markdown(&theme, topic));
    }

    // Anything else walks the command tree to the page it names
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
    output.text(&command.render_long_help().ansi().to_string())
}

/// Lays out the contract with its labels padded to one column and its values
/// wrapped under themselves.
///
/// The examples follow as bare commands, since a pasted prompt breaks in a
/// shell.
fn footer(theme: &Theme, fields: &[(&str, &str)], examples: &str) -> String {
    let column = fields
        .iter()
        .map(|(label, _)| label.len() + 2)
        .max()
        .unwrap_or(0);
    let mut lines = fields
        .iter()
        .map(|(label, text)| {
            style::wrap(
                &format!(
                    "{}{}{}",
                    theme.paint(Role::Muted, format!("{label}:")),
                    " ".repeat(column - label.len() - 1),
                    theme.inline(text)
                ),
                theme.width,
                column,
            )
        })
        .collect::<Vec<_>>();

    // The examples follow under their own heading, styled as commands
    lines.push(format!("\n{}", theme.paint(Role::Heading, "Examples:")));
    lines.extend(examples.lines().map(|line| {
        style::wrap(
            &format!("  {}", theme.paint(Role::Accent, line)),
            theme.width,
            2,
        )
    }));
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
        lines.push(style::wrap(&line, theme.width, hanging));
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
        _ => return None,
    })
}

/// Tests of help pages, contract footers and topic rendering.
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that paragraphs sit between the description and usage on long
    /// pages, and appear on short pages only at the root.
    #[test]
    fn test_paragraphs_follow_descriptions_on_every_command() {
        /// Checks both layouts before walking the command's children.
        fn visit(command: &mut clap::Command, root: bool) {
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
            let (short_opening, _) = short.split_once("\n\nUsage:").unwrap();
            assert_eq!(
                short_opening,
                if root { opening } else { description },
                "{}",
                command.get_name()
            );
            for child in command.get_subcommands_mut() {
                visit(child, false);
            }
        }

        // Both color modes and widths keep the same paragraph boundaries
        for width in [60, 80] {
            for color in [Color::Off, Color::True] {
                let theme = Theme::test(width, color, true);
                visit(&mut command(&theme), true);
            }
        }
    }

    /// Checks the root's screen budget in both help layouts at 80 columns.
    #[test]
    fn test_root_pages_fit_on_a_screen() {
        let theme = Theme::test(80, Color::Off, false);
        let mut root = command(&theme);
        for page in [root.render_help(), root.render_long_help()] {
            let page = page.to_string();
            assert!(page.lines().count() <= 42, "{page}");
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
            "Requires: a paired Ark\nApproval: only if locked\n\n\x1b[1mExamples:\x1b[0m\n  \x1b[1mark unlock\x1b[0m"
        );
        assert_eq!(
            markdown(
                &theme,
                "# States\n\nUse `ark status`.\n\n- Keep the phone nearby.\n\n```sh\nark unlock\n```\n"
            ),
            "\x1b[1mStates\x1b[0m\n\nUse \x1b[1mark status\x1b[0m.\n\n  - Keep the phone nearby.\n\n  \x1b[1mark unlock\x1b[0m"
        );
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
            "      --log <LOG>",
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
        for name in ["data", "app", "firmware"] {
            let group = root.find_subcommand_mut(name).unwrap();
            let long = group.render_long_help().to_string();
            assert!(long.contains("Each subcommand has its own requirements"));
            assert!(!long.contains("Requires:"));
            assert!(!long.contains("Approval:"));
            assert!(!long.contains("Exit:"));
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
