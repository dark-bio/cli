// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Turns clap's parse errors into usage errors in ark's words, keeping clap's
//! near matches as hints.

use crate::{args, error::Error};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use std::ffi::OsString;

/// Builds the usage error for a failed parse of `arguments`, pointing at the
/// command's help when clap has no near match.
pub(crate) fn from_clap(
    error: &clap::Error,
    root: &clap::Command,
    arguments: &[OsString],
) -> Error {
    // Word the message from clap's context rather than its own text
    let path = command_path(root, arguments);
    let argument = context(error, ContextKind::InvalidArg);
    let value = context(error, ContextKind::InvalidValue);
    let option = argument.split_whitespace().next().unwrap_or("argument");
    let message = match error.kind() {
        ErrorKind::InvalidSubcommand => format!(
            "unknown command '{}'",
            context(error, ContextKind::InvalidSubcommand)
        ),
        ErrorKind::UnknownArgument if option.starts_with('-') => {
            format!("unknown option '{argument}'")
        }
        ErrorKind::UnknownArgument => format!("unexpected argument '{argument}'"),
        ErrorKind::MissingRequiredArgument => match error.get(ContextKind::InvalidArg) {
            Some(ContextValue::Strings(arguments)) => arguments
                .iter()
                .map(|argument| missing_argument(&path, argument))
                .collect::<Vec<_>>()
                .join("; "),
            _ => missing_argument(&path, &argument),
        },
        ErrorKind::MissingSubcommand => "missing a command".into(),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation if value.is_empty() => {
            format!("missing a value for {option}")
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => match option {
            "<SLOT>" | "[SLOT]" | "--slot" => format!("unknown slot '{value}'"),
            "<POPULATION>" | "[POPULATION]" => format!("unknown population '{value}'"),
            "--env" => format!("unknown environment '{value}'"),
            "--timeout" => "timeout must be a positive number of seconds".into(),
            "<SHELL>" => format!("unknown shell '{value}'"),
            _ => format!("unknown value '{value}' for {option}"),
        },
        ErrorKind::ArgumentConflict
            if option == "--all" && context(error, ContextKind::PriorArg) == "[POPULATION]" =>
        {
            "[POPULATION] cannot be combined with --all".into()
        }
        ErrorKind::ArgumentConflict => format!(
            "{argument} cannot be combined with {}",
            context(error, ContextKind::PriorArg)
        ),
        _ => "invalid arguments".into(),
    };

    // Keep clap's near matches as hints
    let mut result = Error::new(2, "usage", message);
    for (kind, label) in [
        (ContextKind::SuggestedSubcommand, "command"),
        (ContextKind::SuggestedArg, "option"),
        (ContextKind::SuggestedValue, "value"),
    ] {
        for suggested in suggestions(error, kind) {
            let suggested = if kind == ContextKind::SuggestedSubcommand {
                format!("{path} {suggested}")
            } else {
                suggested
            };
            result
                .hints
                .push(format!("a similar {label} exists: `{suggested}`"));
        }
    }

    // Slots and environments go through custom parsers, and clap misses swapped
    // letters in population codes, so match their spelling here
    if matches!(
        error.kind(),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation
    ) {
        let suggested = match option {
            "<SLOT>" | "[SLOT]" | "--slot" => similar(&value, args::slot_names()),
            "<POPULATION>" | "[POPULATION]" => similar_population(&value),
            "--env" => similar(
                &value,
                args::environments().map(|environment| environment.to_string()),
            ),
            _ => None,
        };
        if let Some(suggested) = suggested {
            let hint = format!("a similar value exists: `{suggested}`");
            if !result.hints.contains(&hint) {
                result.hints.push(hint);
            }
        }
        if matches!(option, "<SLOT>" | "[SLOT]" | "--slot") {
            result
                .hints
                .push("use a slot name or id from `ark data list`".into());
        }
        if matches!(option, "<POPULATION>" | "[POPULATION]") {
            result
                .hints
                .push("use a population code from `ark data samples`".into());
        }
    }
    if result.hints.is_empty() {
        result.hints.push(format!("see `{path} --help`"));
    }
    result
}

/// Finds the population code a mistyped one most likely means.
fn similar_population(value: &str) -> Option<String> {
    use crate::data::samples::POPULATIONS;

    // Try clap's matcher, then the codes with two neighboring letters swapped
    similar(value, POPULATIONS.iter().map(|population| population.code)).or_else(|| {
        let &[a, b, c] = value.as_bytes() else {
            return None;
        };
        POPULATIONS
            .iter()
            .find(|population| {
                population.code.as_bytes() == [b, a, c] || population.code.as_bytes() == [a, c, b]
            })
            .map(|population| population.code.to_owned())
    })
}

/// Words a missing positional by what it is, rather than by its placeholder.
fn missing_argument(path: &str, argument: &str) -> String {
    match (path, argument) {
        ("ark data upload", "<FILE>") => "missing the file to upload".into(),
        ("ark run", "<FILE>") => "missing the app to run".into(),
        (_, "<SLOT>") => "missing the slot name or id".into(),
        ("ark completions", "<SHELL>") => "missing the shell for completions".into(),
        _ => format!("missing {argument}"),
    }
}

/// Reads one value from clap's error context, as plain text.
fn context(error: &clap::Error, kind: ContextKind) -> String {
    error.get(kind).map(ToString::to_string).unwrap_or_default()
}

/// Reads the near matches clap suggests, in its order.
pub(crate) fn suggestions(error: &clap::Error, kind: ContextKind) -> Vec<String> {
    match error.get(kind) {
        Some(ContextValue::String(value)) => vec![value.clone()],
        Some(ContextValue::Strings(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// Finds the near match of `value` among `candidates`, as clap would suggest
/// it.
pub(crate) fn similar(
    value: &str,
    candidates: impl IntoIterator<Item = impl Into<clap::builder::Str>>,
) -> Option<String> {
    use clap::builder::TypedValueParser;
    let parser = clap::builder::PossibleValuesParser::new(candidates.into_iter().map(Into::into));
    let error = parser
        .parse_ref(
            &clap::Command::new("ark"),
            None,
            std::ffi::OsStr::new(value),
        )
        .err()?;
    suggestions(&error, ContextKind::SuggestedValue).pop()
}

/// Finds the command path in `arguments`, skipping options and their values.
fn command_path(root: &clap::Command, arguments: &[OsString]) -> String {
    let mut command = root;
    let mut path = String::from("ark");
    let mut arguments = arguments.iter().skip(1);
    while let Some(value) = arguments.next() {
        let value = value.to_string_lossy();
        if value == "--" {
            break;
        }
        if let Some(long) = value.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let Some(option) = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(name))
            else {
                break;
            };
            if option.get_action().takes_values() && !attached {
                arguments.next();
            }
        } else if let Some(short) = value.strip_prefix('-') {
            for (index, name) in short.char_indices() {
                let Some(option) = command
                    .get_arguments()
                    .find(|arg| arg.get_short() == Some(name))
                else {
                    return path;
                };
                if option.get_action().takes_values() {
                    if index + name.len_utf8() == short.len() {
                        arguments.next();
                    }
                    break;
                }
            }
        } else if let Some(child) = command.find_subcommand(value.as_ref()) {
            path.push(' ');
            path.push_str(child.get_name());
            command = child;
        } else {
            break;
        }
    }
    path
}
