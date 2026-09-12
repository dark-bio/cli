// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Only connect and wire diagnostics enter the CLI's log stream. HTTP and
//! subprocess logging is excluded so authorization headers cannot appear.

use crate::output::Output;
use serde_json::{Map, Value, json};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

pub(crate) fn init(output: Output, verbosity: u8) {
    if verbosity == 0 {
        return;
    }
    let filter = tracing_subscriber::filter::filter_fn(move |metadata| {
        let target = metadata.target();
        (target == "darkbio_connect::setup")
            || (verbosity >= 2
                && target.starts_with("darkbio_connect")
                && *metadata.level() <= tracing::Level::DEBUG)
            || (verbosity >= 3 && target.starts_with("darkbio_wire"))
    });
    let _ = tracing_subscriber::registry()
        .with(Log(output).with_filter(filter))
        .try_init();
}

struct Log(Output);
impl<S: Subscriber> Layer<S> for Log {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields(Map::new());
        event.record(&mut fields);
        let metadata = event.metadata();
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

struct Fields(Map<String, Value>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
}
