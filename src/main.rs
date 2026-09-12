// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Command dispatch; protocol workflows live in darkbio-connect.

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

use args::{Cli, Command};
use clap::{FromArgMatches, Parser};
use context::Context;
use error::Error;
use serde_json::{Value, json};
use std::process::ExitCode;

/// Parses the invocation, installs output and interruption, then reports one outcome.
/// Help and usage failures honor stream formatting even before typed parsing succeeds.
fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let format = requested_format(&arguments);
    let mut command = help::command(&style::Theme::new(format, false));
    let matches = match command.try_get_matches_from_mut(&arguments) {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code() as u8;
            if code != 0 && json_requested(&arguments) {
                let mut options = Cli::parse_from(["ark"]).options;
                options.format = args::Format::Json;
                let output = output::Output::new(&options);
                let message = error.to_string();
                let message = message
                    .split("\n\n")
                    .next()
                    .unwrap_or(&message)
                    .trim_start_matches("error: ")
                    .trim();
                let error = Error::new(2, "usage", message);
                let _ = output.document(&json!({"error":error.json()}));
                output.error(&error);
            } else if code != 0 {
                // Usage errors follow stderr's capabilities, independently of help.
                let mut command = help::command(&style::Theme::new(format, true));
                if let Err(error) = command.try_get_matches_from_mut(&arguments) {
                    let _ = error.print();
                }
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
    logging::init(output.clone(), cli.options.verbose);
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
    let result = if let Err(error) = validation {
        Err(error)
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
            help::command(&style::Theme::new(context.options.format, false)).print_help()?;
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
        Some(Command::Help { path, all }) => help::run(&path, all, context.options.format),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut help::command(&style::Theme::new(args::Format::Text, false)),
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

/// Parsing can fail before clap produces matches. Only the explicit format
/// selection is needed to report that failure in the requested stream format.
fn json_requested(arguments: &[std::ffi::OsString]) -> bool {
    requested_format(arguments) == args::Format::Json
}

/// Finds the last explicit format before --, without requiring valid command syntax.
fn requested_format(arguments: &[std::ffi::OsString]) -> args::Format {
    let mut format = args::Format::Auto;
    let mut arguments = arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        let value = if argument == "--format" {
            arguments.next().and_then(|value| value.to_str())
        } else {
            argument
                .to_str()
                .and_then(|value| value.strip_prefix("--format="))
        };
        if let Some(value) = value {
            format = match value {
                "human" => args::Format::Human,
                "text" => args::Format::Text,
                "json" => args::Format::Json,
                _ => args::Format::Auto,
            };
        }
    }
    format
}
