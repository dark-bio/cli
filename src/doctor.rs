// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Independent diagnostics composed from connection primitives.

use crate::{context::Context, error::Error, firmware::Packages};
use darkbio_connect::schema;
use serde_json::{Value, json};

/// Collects independent diagnostics, skipping checks whose prerequisites failed.
/// Cloud sync is explicit; doctor never pairs or unlocks the Ark to complete checks.
/// The full checklist is printed before the first failure determines the exit class.
pub(crate) fn run(context: &Context) -> Result<(), Error> {
    let mut checks = Checks {
        context,
        rows: Vec::new(),
        failure: None,
    };
    checks.ok(
        "tool",
        &format!(
            "ark {}; connect {}; wire {}",
            env!("CARGO_PKG_VERSION"),
            darkbio_connect::VERSION,
            darkbio_connect::wire::VERSION,
        ),
    );
    let mut discovered = 0;
    for (name, result) in [
        ("usb", darkbio_connect::hardware::list()),
        ("emulator-registry", darkbio_connect::emulator::list()),
    ] {
        match result {
            Ok(devices) => {
                discovered += devices.len();
                checks.ok(name, &format!("{} devices found", devices.len()));
            }
            Err(error) => {
                let error: Error = error.into();
                let error = if name == "usb" && cfg!(target_os = "linux") {
                    error.hint(crate::error::usb_hint())
                } else {
                    error
                };
                checks.fail(name, error);
            }
        }
    }
    checks.ok("devices", &format!("{discovered} Arks discovered"));
    match context.connect_recovery(None) {
        Err(error) => {
            checks.fail("connection", error);
            for name in [
                "compatibility",
                "environment",
                "firmware",
                "sync",
                "registry",
                "pairing",
                "relay",
                "slots",
            ] {
                checks.skip(name, "connection unavailable");
            }
        }
        Ok(connection) => {
            checks.ok("connection", "connected and authenticated");
            let current = match connection.require_current() {
                Ok(()) => {
                    checks.ok("compatibility", "firmware supports this tool");
                    true
                }
                Err(error) => {
                    checks.fail("compatibility", error);
                    false
                }
            };
            if let Some(env) = connection.env {
                checks.ok("environment", &env.to_string());
            } else {
                checks.fail(
                    "environment",
                    Error::new(4, "environment-unknown", "no cloud environment selected")
                        .hint("select one with --env"),
                );
            }
            if connection.env.is_none() {
                checks.skip("firmware", "cloud environment unknown");
            } else {
                match Packages::new(context, connection.env)
                    .and_then(|mut packages| packages.list(context))
                {
                    Ok(firmwares) => checks.ok(
                        "firmware",
                        &format!(
                            "installed {}; newest published {}",
                            connection.info.firmware_version,
                            firmwares
                                .first()
                                .map(|firmware| firmware.version.as_str())
                                .unwrap_or("none")
                        ),
                    ),
                    Err(error) => checks.fail("firmware", error),
                }
            }
            let synced = if connection.env.is_none() {
                checks.skip("sync", "cloud environment unknown");
                false
            } else {
                match connection.client.sync(context.timing()) {
                    Ok(()) => {
                        checks.ok("sync", "cloud keys and signed clock accepted");
                        true
                    }
                    Err(error) => {
                        checks.fail("sync", error.into());
                        false
                    }
                }
            };
            if synced {
                match connection.client.genuine(context.timing()) {
                    Ok(registration) if registration.active() => checks.ok("registry", "active"),
                    Ok(_) => checks.fail(
                        "registry",
                        Error::new(4, "registry-inactive", "registration inactive"),
                    ),
                    Err(error) => checks.fail("registry", error.into()),
                }
            } else {
                checks.skip("registry", "cloud sync unavailable");
            }
            if !current {
                for name in ["pairing", "relay", "slots"] {
                    checks.skip(name, "firmware update required");
                }
            } else {
                let state = &connection.info;
                checks.ok(
                    "pairing",
                    if !state.paired {
                        "unpaired"
                    } else if !state.unlocked {
                        "paired, locked"
                    } else {
                        "paired, unlocked"
                    },
                );
                if synced && state.paired {
                    match connection.client.attach_relay(context.timing()) {
                        Ok(()) => checks.ok("relay", "attached"),
                        Err(error) => checks.fail("relay", error.into()),
                    }
                } else {
                    checks.skip("relay", "requires a paired Ark and cloud sync");
                }
                if synced && state.unlocked {
                    match connection
                        .client
                        .call(schema::SlotListRequest {}, context.timing())
                    {
                        Ok(slots) => {
                            checks.ok("slots", &format!("{} slots readable", slots.slots.len()))
                        }
                        Err(error) => checks.fail("slots", error.into()),
                    }
                } else {
                    checks.skip("slots", "requires an unlocked Ark and cloud sync");
                }
            }
        }
    }
    let cache = crate::data::cache::directory();
    match crate::data::cache::size(&cache) {
        Ok(size) => checks.ok(
            "cache",
            &format!("{} ({})", cache.display(), crate::style::bytes(size)),
        ),
        Err(error) => checks.fail("cache", error.into()),
    }
    let mut document = crate::versions();
    document["checks"] = json!(checks.rows);
    context.output.checklist(&document, &checks.rows)?;
    checks.failure.map_or(Ok(()), Err)
}
/// Ordered diagnostic results and the first failure used for command status.
struct Checks<'a> {
    /// Invocation output used to emit optional step diagnostics.
    context: &'a Context,
    /// Checks in execution order, including explicit skips and local hints.
    rows: Vec<Value>,
    /// First failed check, retained while later independent checks continue.
    failure: Option<Error>,
}
impl Checks<'_> {
    /// Records a successful diagnostic with its observed detail.
    fn ok(&mut self, name: &str, detail: &str) {
        self.add(name, "ok", detail, None);
    }
    /// Records an unmet prerequisite without making the command fail by itself.
    fn skip(&mut self, name: &str, detail: &str) {
        self.add(name, "skip", detail, None);
    }
    /// Records the failure and first hint, preserving the earliest command error.
    fn fail(&mut self, name: &str, error: Error) {
        self.add(
            name,
            "fail",
            &error.message,
            error.hints.first().map(String::as_str),
        );
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
    /// Appends one structured check and its optional verbose event.
    fn add(&mut self, name: &str, result: &str, detail: &str, hint: Option<&str>) {
        self.context
            .output
            .event("step", format!("{name}: {result}, {detail}"));
        self.rows
            .push(json!({"name":name,"result":result,"detail":detail,"hint":hint}));
    }
}
