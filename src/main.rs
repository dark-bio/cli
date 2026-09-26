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

#[cfg(test)]
mod testing;

use args::{Cli, Command};
use clap::{FromArgMatches, Parser};
use context::Context;
use darkbio_clock::Clock;
use error::Error;
use serde_json::{Value, json};
use std::process::ExitCode;

/// Parses the invocation, installs output and interruption, then reports one outcome.
/// Help and usage failures honor stream formatting even before typed parsing succeeds.
fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 2 && arguments[1] == update::ENTRY_POINT {
        update::run();
        return ExitCode::SUCCESS;
    }
    let json = arguments
        .iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let mut command = help::command(&help::theme());
    let matches = match command.try_get_matches_from_mut(&arguments) {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code() as u8;
            if code != 0 {
                let mut options = Cli::parse_from(["ark"]).options;
                options.json = json;
                let output = output::Output::new(&options);
                let message = error.to_string();
                let message = message
                    .split("\n\n")
                    .next()
                    .unwrap_or(&message)
                    .trim_start_matches("error: ")
                    .trim();
                let error = Error::new(2, "usage", message);
                if output.json() {
                    let _ = output.document(&json!({"error":error.json()}));
                }
                output.error(&error);
            } else {
                let _ = error.print();
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
    let output = output::Output::new(&cli.options);
    logging::init(output.clone(), cli.options.verbose, cli.options.log);
    let interrupt = match interrupt::Interrupt::install(output.clone()) {
        Ok(interrupt) => interrupt,
        Err(error) => {
            output.error(&error);
            return ExitCode::from(error.class);
        }
    };
    let validation = cli.validate();
    let context = Context {
        options: cli.options,
        output,
        interrupt,
    };
    // Valid commands print the release note, except help, completions, --version, a bare run and doctor.
    // The note comes before any connection, so it reads the real clock.
    if validation.is_ok()
        && !cli.help
        && !cli.version
        && !matches!(
            &cli.command,
            None | Some(Command::Help { .. } | Command::Completions { .. } | Command::Doctor)
        )
    {
        update::start(
            &context.output,
            chrono::DateTime::from(Clock::real().system_time()),
        );
    }
    let result = if let Err(error) = validation {
        Err(error)
    } else if cli.help {
        help::run(&[], cli.all)
    } else if cli.version {
        context.output.document(&versions())
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
/// Dispatches one command; an absent command prints top-level help without discovery.
fn run(context: &Context, command: Option<Command>) -> Result<(), Error> {
    match command {
        None => {
            help::command(&help::theme()).print_help()?;
            Ok(())
        }
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
        Some(Command::Help { path, all }) => help::run(&path, all),
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
/// Reports compiled crate versions and the firmware compatibility baseline.
pub(crate) fn versions() -> Value {
    json!({
        "tool": env!("CARGO_PKG_VERSION"),
        "connect": darkbio_connect::VERSION,
        "wire": darkbio_connect::wire::VERSION,
        "minimum_firmware": firmware::MINIMUM_VERSION,
        "minimum_develop_publish": device::timestamp(firmware::MINIMUM_DEVELOP_PUBLISH),
    })
}
