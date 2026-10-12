// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Command dispatch; protocol workflows live in the internal connection library.

mod access;
mod args;
mod context;
mod data;
mod device;
mod doctor;
mod error;
mod execution;
mod firmware;
mod help;
mod http;
mod interrupt;
mod logging;
mod output;
mod pairing;
mod progress;
mod style;
mod update;
mod usage;

#[cfg(test)]
mod testing;

use args::{Cli, Command};
use clap::{FromArgMatches, Parser};
use context::Context;
use darkbio_clock::Clock;
use error::Error;
use serde_json::json;
use std::process::ExitCode;

/// Version this build prints, as published, or the Cargo version marked
/// `-local`.
pub(crate) const VERSION: &str = env!("ARK_BUILD_VERSION");

/// Parses the invocation, installs output and interruption handling, then
/// reports one outcome.
///
/// Help and usage failures honor stream formatting even before typed parsing
/// succeeds.
fn main() -> ExitCode {
    // A hidden sole argument runs the detached release lookup instead
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 2 && arguments[1] == update::ENTRY_POINT {
        update::run();
        return ExitCode::SUCCESS;
    }

    // Find --json by hand, so a usage failure can still answer in JSON
    let json = arguments
        .iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");

    // Parse with the themed command. A clap display that exits cleanly prints
    // as it is, and any other failure is a usage error.
    let mut command = help::command(&help::theme());
    let matches = match command.try_get_matches_from_mut(&arguments) {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code() as u8;
            let mut options = Cli::parse_from(["ark"]).options;
            options.json = json;
            let output = output::Output::new(&options);
            if code != 0 {
                let error = usage::from_clap(&error, &command, &arguments);
                if output.json() {
                    let _ = output.document(&json!({"error":error.json()}));
                }
                output.error(&error);
            } else if let Err(error) = output.text(&error.render().ansi().to_string()) {
                output.error(&error);
                return ExitCode::from(error.class);
            }
            return ExitCode::from(code);
        }
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(2);
        }
    };

    // Install output, logging and interruption handling before any work
    let output = output::Output::new(&cli.options);
    logging::init(output.clone(), cli.options.verbose, cli.options.log);
    let interrupt = match interrupt::Interrupt::install(output.clone()) {
        Ok(interrupt) => interrupt,
        Err(error) => {
            output.error(&error);
            return ExitCode::from(error.class);
        }
    };

    // A failed validation becomes the result, reported like any command failure
    let validation = cli.validate();
    let check_update = validation.is_ok() && checks_for_update(&cli);
    let context = Context {
        options: cli.options,
        output,
        interrupt,
    };

    // The release note precedes the command and any connection, so it reads
    // the real clock
    if check_update {
        update::start(
            &context.output,
            chrono::DateTime::from(Clock::real().system_time()),
        );
    }

    // Run the command, or print the top-level help or the version when asked
    let result = if let Err(error) = validation {
        Err(error)
    } else if cli.help {
        help::run(&context.output, &[], cli.all)
    } else if cli.version {
        context
            .output
            .document_with(&json!({"version": VERSION}), |_| format!("ark {VERSION}"))
    } else {
        run(&context, cli.command)
    };

    // Wait for interruption cleanup already in progress. Stop the live line
    // before printing an error, preserving a result already emitted by a command.
    context.interrupt.finished();
    context.output.finish();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if context.output.json() && !context.output.printed() {
                let _ = context.output.document(&json!({"error":error.json()}));
            }
            context.output.error(&error);
            ExitCode::from(error.class)
        }
    }
}

/// Selects the invocations that print the release note and refresh its answer.
///
/// Help, completions, the version and a bare `ark` keep their output to
/// themselves, and doctor reports releases in its own check. The caller checks
/// that the invocation is valid first.
fn checks_for_update(cli: &Cli) -> bool {
    !cli.help
        && !cli.version
        && !matches!(
            &cli.command,
            None | Some(Command::Help { .. } | Command::Completions { .. } | Command::Doctor)
        )
}

/// Dispatches one command; an absent command prints top-level help without
/// discovery.
fn run(context: &Context, command: Option<Command>) -> Result<(), Error> {
    match command {
        None => context.output.text(
            &help::command(&help::theme())
                .render_help()
                .ansi()
                .to_string(),
        ),
        Some(Command::Devices) => device::devices(context),
        Some(Command::Status(args)) => device::status(context, args),
        Some(Command::Genuine) => device::genuine(context),
        Some(Command::Unlock) => device::unlock(context),
        Some(Command::Enroll(args)) => device::enroll(context, args),
        Some(Command::Data(command)) => data::run(context, command),
        Some(Command::App(command)) => execution::run(context, command),
        Some(Command::Firmware(command)) => firmware::run(context, command),
        Some(Command::Pair) => pairing::run(context),
        Some(Command::Doctor) => doctor::run(context),
        Some(Command::Help { path, all }) => help::run(&context.output, &path, all),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut help::command(&help::theme()),
                "ark",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}

/// Tests of which invocations print the release note.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Color, Theme};

    /// Commands may print the release note, while help, completions, the
    /// version, the doctor and invalid invocations never do, whatever this
    /// test's build.
    #[test]
    fn test_release_notes_follow_invocation_policy() {
        let theme = Theme::test(80, Color::Off, true);
        for (args, expected) in [
            (vec!["devices"], true),
            (vec!["status"], true),
            (vec!["status", "--json"], true),
            (vec!["status", "--quiet"], true),
            (vec!["data", "list"], true),
            (vec!["firmware", "list"], true),
            (vec![], false),
            (vec!["--json"], false),
            (vec!["-h"], false),
            (vec!["--help"], false),
            (vec!["--help", "--all"], false),
            (vec!["help"], false),
            (vec!["help", "--all"], false),
            (vec!["help", "status"], false),
            (vec!["status", "-h"], false),
            (vec!["status", "--help"], false),
            (vec!["completions", "zsh"], false),
            (vec!["--version"], false),
            (vec!["-V", "--json"], false),
            (vec!["doctor"], false),
            (vec!["doctor", "--json"], false),
            (vec!["--all"], false),
            (vec!["--timeout", "0", "status"], false),
            (vec!["bogus"], false),
            (vec!["--version", "status"], false),
            (vec!["data", "upload", "x", "--dry-run", "--unlock"], false),
        ] {
            let cli = help::command(&theme)
                .try_get_matches_from(std::iter::once("ark").chain(args.iter().copied()))
                .and_then(|matches| Cli::from_arg_matches(&matches));
            let check_update =
                cli.is_ok_and(|cli| cli.validate().is_ok() && checks_for_update(&cli));
            assert_eq!(check_update, expected, "{args:?}");
        }
    }
}
