// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Tracing subscriber that routes allowlisted diagnostics to the CLI's stderr.
//!
//! Only update, connect and wire diagnostics enter the CLI's log stream. HTTP
//! and subprocess logging is excluded so authorization headers cannot appear.

use crate::{args::Log as Level, output::Output};
use serde_json::{Map, Value, json};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

/// Installs an allowlisted subscriber for steps and requested diagnostics,
/// when `-v` or `--log` asks for either.
///
/// An existing process subscriber is retained if installation is unavailable.
pub(crate) fn init(output: Output, verbose: bool, level: Option<Level>) {
    if !verbose && level.is_none() {
        return;
    }
    let filter = tracing_subscriber::filter::filter_fn(move |metadata| {
        enabled(metadata.target(), *metadata.level(), verbose, level)
    });
    let _ = tracing_subscriber::registry()
        .with(Log(output).with_filter(filter))
        .try_init();
}

/// Rejects every target outside update, connect and wire, regardless of
/// diagnostic level.
///
/// Setup messages from the connection library pass only with `-v`, whatever the
/// log level. A debug log passes update and connect events up to debug level,
/// and a trace log passes update, connect and wire events at every level.
fn enabled(target: &str, severity: tracing::Level, verbose: bool, level: Option<Level>) -> bool {
    if target == "darkbio_connect::setup" {
        return verbose;
    }
    match level {
        Some(Level::Debug) => {
            (target == "ark::update"
                || target == "darkbio_connect"
                || target.starts_with("darkbio_connect::"))
                && severity <= tracing::Level::DEBUG
        }
        Some(Level::Trace) => {
            target == "ark::update"
                || ["darkbio_connect", "darkbio_wire"].iter().any(|name| {
                    target == *name
                        || target
                            .strip_prefix(name)
                            .is_some_and(|suffix| suffix.starts_with("::"))
                })
        }
        None => false,
    }
}

/// Tracing layer that routes selected events through the CLI's stderr policy.
struct Log(Output);

impl<S: Subscriber> Layer<S> for Log {
    /// Renders setup messages as steps and other selected events as structured
    /// or text logs.
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields(Map::new());
        event.record(&mut fields);
        let metadata = event.metadata();

        // Setup messages become steps, and other events JSON or text log lines
        if metadata.target() == "darkbio_connect::setup" {
            if let Some(message) = fields.0.get("message").and_then(Value::as_str) {
                self.0.event("step", message);
            }
        } else if self.0.json() {
            self.0.event_value(
                json!({"event":"log", "level":metadata.level().as_str().to_ascii_lowercase(),
                "target":metadata.target(), "fields":fields.0}),
            );
        } else {
            let message = fields
                .0
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    if key == "message" {
                        value
                    } else {
                        format!("{key}={value}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            self.0.event(
                "log",
                format!("{} {}: {message}", metadata.level(), metadata.target()),
            );
        }
    }
}

/// Collected tracing fields, preserving string values for message rendering.
struct Fields(Map<String, Value>);

impl Visit for Fields {
    /// Stores a debug-only field as its printable representation.
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }

    /// Retains a string field without adding debug quotes.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
}

/// Tests of the diagnostic allowlist.
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that diagnostic selection includes update failures without
    /// exposing HTTP or subprocess logs.
    #[test]
    fn test_diagnostics_are_separate_from_steps_and_exclude_http_and_subprocesses() {
        // Update logs follow --log, setup steps follow -v, and foreign targets
        // never pass
        for level in [None, Some(Level::Debug), Some(Level::Trace)] {
            for verbose in [false, true] {
                assert_eq!(
                    enabled("ark::update", tracing::Level::DEBUG, verbose, level),
                    level.is_some(),
                    "{level:?}, verbose={verbose}"
                );
                assert_eq!(
                    enabled(
                        "darkbio_connect::setup",
                        tracing::Level::INFO,
                        verbose,
                        level
                    ),
                    verbose
                );
                for target in [
                    "ureq",
                    "hyper::client",
                    "rustls",
                    "ark::access",
                    "ark::update_http",
                    "std::process",
                    "darkbio_connect_http",
                ] {
                    assert!(
                        !enabled(target, tracing::Level::ERROR, verbose, level),
                        "{target}"
                    );
                }
            }
        }

        // Debug passes connect up to debug level, and trace adds wire at every
        // level
        assert!(enabled(
            "darkbio_connect::hardware",
            tracing::Level::DEBUG,
            false,
            Some(Level::Debug)
        ));
        assert!(!enabled(
            "darkbio_connect::hardware",
            tracing::Level::TRACE,
            true,
            Some(Level::Debug)
        ));
        assert!(!enabled(
            "darkbio_wire::transport",
            tracing::Level::DEBUG,
            true,
            Some(Level::Debug)
        ));
        for target in ["darkbio_connect::hardware", "darkbio_wire::transport"] {
            assert!(enabled(
                target,
                tracing::Level::TRACE,
                false,
                Some(Level::Trace)
            ));
        }
    }
}
