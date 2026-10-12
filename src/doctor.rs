// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Independent diagnostics composed from connection primitives.

use crate::{context::Context, error::Error, firmware::Packages, output::Output, update};
use darkbio_clock::Clock;
use darkbio_connect::schema;
use semver::Version;
use serde_json::{Value, json};
use std::path::Path;

/// Collects independent diagnostics, skipping checks whose prerequisites failed.
///
/// Cloud sync is explicit, and doctor never pairs or unlocks the Ark to complete
/// a check. The full checklist is printed before the first failure determines
/// the exit class.
pub(crate) fn run(context: &Context) -> Result<(), Error> {
    // Start with the tool's build version and whether a newer release exists
    let mut checks = Checks {
        output: &context.output,
        rows: Vec::new(),
        failure: None,
    };
    checks.ark(
        crate::VERSION,
        cfg!(ark_local),
        update::disabled(),
        |channel| {
            update::refresh(
                &crate::data::cache::directory(),
                channel,
                chrono::DateTime::from(Clock::real().system_time()),
                std::time::Duration::from_secs(context.options.timeout),
            )
        },
    );

    // Each discovery source reports on its own, so one failure keeps the other's
    // result
    for (name, result) in [
        ("hardware", darkbio_connect::hardware::list()),
        ("emulator", darkbio_connect::emulator::list()),
    ] {
        match result {
            Ok(devices) => {
                let count = devices.len();
                let detail = match (name, count) {
                    ("hardware", 0) => "none attached".into(),
                    ("hardware", 1) => "1 Ark".into(),
                    ("hardware", _) => format!("{count} Arks"),
                    (_, 0) => "none running".into(),
                    _ => format!("{count} running"),
                };
                checks.ok(name, &detail);
            }
            Err(error) => {
                let error: Error = error.into();
                let error = if name == "hardware" && cfg!(target_os = "linux") {
                    error.hint(crate::error::usb_hint())
                } else {
                    error
                };
                checks.fail(name, error);
            }
        }
    }

    // Measure the reference cache before moving on to the Ark
    let cache = crate::data::cache::directory();
    let home = directories::BaseDirs::new();
    match crate::data::cache::size(&cache) {
        Ok(size) => checks.ok(
            "cache",
            &cache_detail(&cache, size, home.as_ref().map(|dirs| dirs.home_dir())),
        ),
        Err(error) => checks.fail("cache", error.into()),
    }
    let computer_checks = checks.rows.len();

    // Connect past the compatibility gate, so outdated firmware is checked too,
    // and skip every device check when that fails
    let mut name = "Ark".to_string();
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
                checks.skip(name, "skipped without a connection");
            }
        }
        Ok(connection) => {
            // Report the connection, the firmware's compatibility and the
            // environment
            name = connection.device.name().unwrap_or("Ark").to_string();
            checks.ok("connection", "authenticated");
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

            // The firmware catalog and the cloud sync need a known environment
            if connection.env.is_none() {
                checks.skip("firmware", "needs a known cloud environment");
            } else {
                match Packages::new(context, connection.client.clock(), connection.env)
                    .and_then(|mut packages| packages.list(context))
                {
                    Ok(firmwares) => checks.ok(
                        "firmware",
                        &firmware_detail(
                            &connection.info.firmware_version,
                            firmwares.first().map(|firmware| firmware.version.as_str()),
                        ),
                    ),
                    Err(error) => checks.fail("firmware", error),
                }
            }
            let synced = if connection.env.is_none() {
                checks.skip("sync", "needs a known cloud environment");
                false
            } else {
                match connection.client.sync(context.timing()) {
                    Ok(()) => {
                        checks.ok("sync", "keys and clock accepted");
                        true
                    }
                    Err(error) => {
                        checks.fail("sync", error.into());
                        false
                    }
                }
            };

            // The registry answers only after a successful sync
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
                checks.skip("registry", "needs cloud sync");
            }

            // The pairing, relay and slot checks need supported firmware, and
            // take the pairing and lock state as they find it
            if !current {
                for name in ["pairing", "relay", "slots"] {
                    checks.skip(name, "needs a firmware update");
                }
            } else {
                let state = &connection.info;
                checks.ok(
                    "pairing",
                    if !state.paired {
                        "not paired"
                    } else if !state.unlocked {
                        "paired and locked"
                    } else {
                        "paired and unlocked"
                    },
                );
                if synced && state.paired {
                    match connection.client.attach_relay(context.timing()) {
                        Ok(()) => checks.ok("relay", "attached"),
                        Err(error) => checks.fail("relay", error.into()),
                    }
                } else {
                    checks.skip("relay", "needs a paired Ark and cloud sync");
                }
                if synced && state.unlocked {
                    match connection
                        .client
                        .call(schema::SlotListRequest {}, context.timing())
                    {
                        Ok(slots) => checks.ok("slots", &format!("{} readable", slots.slots.len())),
                        Err(error) => checks.fail("slots", error.into()),
                    }
                } else {
                    checks.skip("slots", "needs an unlocked Ark and cloud sync");
                }
            }
        }
    }

    // Print the whole checklist, then fail with the first failure
    let document = json!({"version": crate::VERSION, "checks": checks.rows});
    context.output.checklist(
        &document,
        &[
            ("This computer", &checks.rows[..computer_checks]),
            (&name, &checks.rows[computer_checks..]),
        ],
    )?;
    checks.failure.map_or(Ok(()), Err)
}

/// Describes the installed firmware against the newest catalog entry.
fn firmware_detail(installed: &str, newest: Option<&str>) -> String {
    match newest {
        Some(newest) if newest == installed => format!("{installed}, the newest published"),
        Some(newest) => format!("{installed}, and {newest} is published"),
        None => format!("{installed}, none published"),
    }
}

/// Describes the cache's size and location, shortening the home directory.
fn cache_detail(path: &Path, size: u64, home: Option<&Path>) -> String {
    let path = home
        .and_then(|home| path.strip_prefix(home).ok())
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                Path::new("~").to_path_buf()
            } else {
                Path::new("~").join(relative)
            }
        })
        .unwrap_or_else(|| path.to_path_buf());
    format!("{} in {}", crate::style::bytes(size), path.display())
}

/// Ordered diagnostic results and the first failure used for command status.
struct Checks<'a> {
    /// Invocation output, for verbose step diagnostics.
    output: &'a Output,
    /// Checks in execution order, including explicit skips and local hints.
    rows: Vec<Value>,
    /// First failed check, retained while later independent checks continue.
    failure: Option<Error>,
}

impl Checks<'_> {
    /// Records the build version and looks up the newest ark when permitted.
    ///
    /// A newer one is a warn and a failed lookup a skip, so neither sets the
    /// exit code.
    fn ark(
        &mut self,
        version: &str,
        local: bool,
        ci: bool,
        lookup: impl FnOnce(update::Channel) -> Result<Version, &'static str>,
    ) {
        // Local builds and CI never invoke the release lookup
        if local {
            self.skip(
                "ark",
                &format!("{version}, not looked up for a local build"),
            );
            return;
        }
        if ci {
            self.skip("ark", &format!("{version}, not looked up under CI"));
            return;
        }

        // Compare semantic precedence on the build's own release channel
        let running = Version::parse(version).expect("build version is semver");
        let channel = update::Channel::for_version(&running);
        match lookup(channel) {
            Ok(newest) if newest.cmp_precedence(&running).is_gt() => self.warn(
                "ark",
                &format!("{version}, and {newest} is out"),
                &update::hint(channel),
            ),
            Ok(_) => self.ok(
                "ark",
                &format!("{version}, the newest {}", channel.description()),
            ),
            Err(error) => self.skip("ark", &format!("{version}, release lookup failed: {error}")),
        }
    }

    /// Records a successful diagnostic with its observed detail.
    fn ok(&mut self, name: &str, detail: &str) {
        self.add(name, "ok", detail, None);
    }

    /// Records something to act on with its hint, never failing the command.
    fn warn(&mut self, name: &str, detail: &str, hint: &str) {
        self.add(name, "warn", detail, Some(hint));
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
        self.output
            .event("step", format!("{name}: {result}, {detail}"));
        self.rows
            .push(json!({"name":name,"result":result,"detail":detail,"hint":hint}));
    }
}

/// Tests of release outcomes and the details shared by reading and JSON reports.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Cli;
    use clap::Parser;

    /// The ark check includes the build version in every outcome and only looks
    /// up published builds outside CI.
    #[test]
    fn test_ark_check_release_states() {
        let output = Output::new(&Cli::parse_from(["ark"]).options);
        for (case, version, local, ci, newest, result, detail) in [
            (
                "newest release",
                "0.4.1",
                false,
                false,
                Ok("0.4.1"),
                "ok",
                "0.4.1, the newest release",
            ),
            (
                "newer release",
                "0.4.1",
                false,
                false,
                Ok("0.4.2"),
                "warn",
                "0.4.1, and 0.4.2 is out",
            ),
            (
                "newest development build",
                "0.4.2-dev.7",
                false,
                false,
                Ok("0.4.2-dev.7"),
                "ok",
                "0.4.2-dev.7, the newest development build",
            ),
            (
                "newer development build",
                "0.4.2-dev.7",
                false,
                false,
                Ok("0.4.2-dev.8"),
                "warn",
                "0.4.2-dev.7, and 0.4.2-dev.8 is out",
            ),
            (
                "lookup failure",
                "0.4.1",
                false,
                false,
                Err("GitHub could not be reached"),
                "skip",
                "0.4.1, release lookup failed: GitHub could not be reached",
            ),
            (
                "CI",
                "0.4.1",
                false,
                true,
                Ok("0.4.2"),
                "skip",
                "0.4.1, not looked up under CI",
            ),
            (
                "local build",
                "0.4.1-local",
                true,
                false,
                Ok("0.4.2"),
                "skip",
                "0.4.1-local, not looked up for a local build",
            ),
            (
                "local build under CI",
                "0.4.1-local",
                true,
                true,
                Ok("0.4.2"),
                "skip",
                "0.4.1-local, not looked up for a local build",
            ),
        ] {
            // Supply the answer directly, without a clock or a network lookup
            let mut checks = Checks {
                output: &output,
                rows: Vec::new(),
                failure: None,
            };
            let mut called = false;
            checks.ark(version, local, ci, |channel| {
                called = true;
                assert_eq!(
                    channel,
                    if version.contains("-dev.") {
                        update::Channel::Develop
                    } else {
                        update::Channel::Release
                    },
                    "{case}"
                );
                newest.map(|version| Version::parse(version).unwrap())
            });

            // Only a newer release has a hint, and no release outcome fails
            assert_eq!(called, !local && !ci, "{case}");
            assert_eq!(checks.rows.len(), 1, "{case}");
            let row = &checks.rows[0];
            assert_eq!(row["name"], "ark", "{case}");
            assert_eq!(row["result"], result, "{case}");
            assert_eq!(row["detail"], detail, "{case}");
            if result == "warn" {
                assert!(
                    row["hint"].as_str().is_some_and(|hint| !hint.is_empty()),
                    "{case}"
                );
            } else {
                assert!(row["hint"].is_null(), "{case}");
            }
            assert!(checks.failure.is_none(), "{case}");
        }
    }

    /// Firmware details distinguish a matching build, a different one and an
    /// empty catalog without claiming an upgrade is required.
    #[test]
    fn test_firmware_details_describe_the_catalog() {
        for (newest, expected) in [
            (Some("0.12.0-abc"), "0.12.0-abc, the newest published"),
            (
                Some("0.12.1-def"),
                "0.12.0-abc, and 0.12.1-def is published",
            ),
            (None, "0.12.0-abc, none published"),
        ] {
            assert_eq!(
                firmware_detail("0.12.0-abc", newest),
                expected,
                "{newest:?}"
            );
        }
    }

    /// Cache paths shorten only a complete home-directory prefix and keep the
    /// size's unit in the detail.
    #[test]
    fn test_cache_details_shorten_the_home_directory() {
        let home = Path::new("/home/sample");
        let cache = home.join("cache");
        assert_eq!(
            cache_detail(&cache, 2048, Some(home)),
            format!("2.0 KiB in {}", Path::new("~").join("cache").display())
        );
        assert_eq!(cache_detail(home, 0, Some(home)), "0 B in ~");

        // A sibling with the same text prefix is not inside the home directory
        let sibling = Path::new("/home/sample-other/cache");
        for home in [Some(home), None] {
            assert_eq!(
                cache_detail(sibling, 2048, home),
                format!("2.0 KiB in {}", sibling.display()),
                "{home:?}"
            );
        }
    }
}
