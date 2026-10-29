// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Firmware plans, package downloads and verification after reboot.

mod package;

use crate::{
    access, args,
    context::{Connection, Context},
    error::Error,
    http,
    output::{Output, human},
    progress::{Transfer, Update},
    style::{self, Theme},
};
use darkbio_clock::Clock;
use darkbio_connect::{UpdateProgress, schema, trust::Environment};
use package::Package;
use serde_json::{Value, json};
use std::time::Duration;

/// Verification window covering old-session closure and discovery after
/// installation.
pub(crate) const REBOOT_WAIT: Duration = Duration::from_secs(120);

/// First released firmware that pulls app modules by digest, which `ark run` needs.
pub(crate) const MINIMUM_VERSION: &str = "0.14.0";

/// Earliest publish time of a develop build this CLI accepts, in Unix seconds.
///
/// It moves forward whenever a change on the current release needs developers
/// to rebuild their image.
pub(crate) const MINIMUM_DEVELOP_PUBLISH: u64 = 1_793_267_015; // 2026-10-29 09:43:35 UTC

/// Checks that the firmware is at least [`MINIMUM_VERSION`], and that a
/// mutable develop build has a publish time at or after
/// [`MINIMUM_DEVELOP_PUBLISH`].
///
/// This is CLI compatibility guidance based on reported firmware metadata.
pub(crate) fn check_compatibility(info: &schema::DeviceInfoResponse) -> Result<(), Error> {
    // A version below the minimum, or one that does not parse, needs an update
    let minimum = package::Version::parse(&format!("{MINIMUM_VERSION}-develop"))
        .expect("compiled firmware minimum is valid");
    let Some(version) = package::Version::parse(&info.firmware_version)
        .ok()
        .filter(|version| *version >= minimum)
    else {
        return Err(Error::new(
            5,
            "firmware-outdated",
            format!(
                "firmware {} requires an update; minimum {MINIMUM_VERSION}",
                info.firmware_version,
            ),
        ));
    };

    // A develop build also needs a publish time at or after the cutoff
    if version.is_develop() && info.firmware_publish < MINIMUM_DEVELOP_PUBLISH {
        return Err(Error::new(
            5,
            "firmware-outdated",
            "develop build is outdated; rebuild or update the firmware",
        ));
    }
    Ok(())
}

/// Lists candidates or plans and installs a selected firmware archive.
///
/// The CLI owns package retrieval, consent and reboot verification; connect owns the
/// authorized update sequence. Partial results distinguish installation from return.
pub(crate) fn run(context: &Context, command: args::Firmware) -> Result<(), Error> {
    // Connect past the compatibility gate, which an update exists to cross, and
    // fetch the published packages
    let connection = context.connect_recovery(None)?;
    let clock = connection.client.clock();
    let mut packages = Packages::new(context, clock.clone(), connection.env)?;
    let firmwares = packages.list(context)?;
    if let args::Firmware::List = command {
        return listing(
            &context.output,
            &connection.info.firmware_version,
            &firmwares,
        );
    }

    // Plan the update, and print the plan alone when nothing is newer or on a
    // dry run
    let args::Firmware::Update {
        version,
        dry_run,
        no_wait,
        yes,
    } = command
    else {
        unreachable!()
    };
    let target = select(
        &firmwares,
        version.as_deref(),
        &connection.info.firmware_version,
    )?;
    let mut approval = approval(&connection.info);
    let mut value = json!({
        "from": connection.info.firmware_version,
        "to": target.map(|firmware| &firmware.version),
        "size_bytes": target.map(|firmware| firmware.size),
        "approval": approval,
        "installed": false,
        "returned": false,
        "verified": false,
        "running": connection.info.firmware_version,
    });
    let Some(target) = target else {
        context
            .output
            .event("note", "no newer firmware is published");
        return context.output.document(&value);
    };
    if dry_run {
        return context.output.document(&value);
    }

    // Announce the update, which then needs --yes or a confirmation at the
    // prompt
    context.output.event(
        "note",
        format!(
            "{} to {}, {}: {}",
            connection.info.firmware_version,
            target.version,
            style::bytes(target.size),
            package::headline(&target.summary)
        ),
    );
    if !yes
        && (!context.interactive()
            || !context.confirm(
                &format!("Install {} and restart the Ark?", target.version),
                false,
            )?)
    {
        context.output.document(&value)?;
        return Err(Error::new(
            1,
            "confirmation-required",
            "firmware installation requires confirmation",
        )
        .hint("add --yes to confirm installation and reboot"));
    }

    // With --unlock, a locked or unreported Ark unlocks first, so the phone
    // approves the update
    if context.options.unlock && matches!(approval, Some(Approval::Button) | None) {
        context.unlock(&connection)?;
        approval = Some(Approval::Phone);
        value["approval"] = json!(approval);
    }

    // The public archive is opened lazily, after the Ark accepts preparation.
    // Connect owns authorization, transfer, verification and installation.
    let mut reader = Download {
        packages: &mut packages,
        context,
        path: package::path(target),
        response: None,
    };
    let mut narration = Narration::new(&context.output, clock.clone(), target.size, approval);
    let result = connection.client.update_firmware(
        &target.firmware(),
        &mut reader,
        context.timing(),
        |stage| narration.update(stage),
    );

    // A failed update prints the plan, with hints for a stale proof or the
    // emulator
    if let Err(error) = result {
        let mut error: Error = error.into();
        context.output.fail_stage(Some(&error));
        context.output.document(&value)?;
        if error.code == "proof-rejected" {
            error.hints = vec!["cloud keys refreshed; retry the firmware update".into()];
        }
        if error.code == "unsupported"
            && connection.device.kind() == darkbio_connect::DeviceKind::Emulator
        {
            error
                .hints
                .push("the emulator app carries its firmware; update the emulator app".into());
        }
        return Err(error);
    }

    // Record the installation, with the running build unknown until the Ark
    // returns
    context.output.end_stage(None, None);
    value["installed"] = json!(true);
    value["running"] = Value::Null;
    context.interrupt.partial(value.clone());

    // Wait for the Ark to return and verify its build, unless --no-wait skips
    // that
    if !no_wait {
        let result = verify_reboot(context, &connection, &target.version, &mut value);
        if let Err(error) = &result {
            context.output.fail_stage(Some(error));
        } else {
            context
                .output
                .end_stage(Some(&format!("Restarted on {}", target.version)), None);
        }
        context.output.document(&value)?;
        return result;
    }
    context.output.document(&value)
}

/// Narration of a firmware update, from its approval through the install.
struct Narration<'a> {
    /// Output the narration goes to, shared with the restart wait.
    output: &'a Output,
    /// Approval the Ark is expected to ask for, which the Ark decides.
    approval: Option<Approval>,
    /// Rate and time left of sending the firmware.
    transfer: Transfer,
}

impl<'a> Narration<'a> {
    /// Prepares to narrate sending `size` bytes after `approval`.
    fn new(output: &'a Output, clock: Clock, size: u64, approval: Option<Approval>) -> Self {
        Self {
            output,
            approval,
            transfer: Transfer::new(
                output.terminal(),
                clock,
                Update {
                    completed: Some(format!("Sent {}", style::bytes(size))),
                    ..Update::new("sending", "Sending")
                },
            ),
        }
    }

    /// Narrates one step of the update, as an approval or a stage.
    fn update(&mut self, stage: UpdateProgress) {
        match stage {
            UpdateProgress::Preparing => match self.approval {
                Some(Approval::None) => {}
                Some(Approval::Phone) => self.output.approve("update", Some("phone"), "Approve the update on your phone, in Ark Companion"),
                Some(Approval::Button) => self.output.approve("update", Some("button"), "Approve the update on the Ark. Press its button with the pin that came with it, in the pinhole under the bottom right LED"),
                None => self.output.approve("update", None, "Approve the update if the Ark asks, on your phone or with its button"),
            },
            UpdateProgress::Uploading { uploaded, total } => {
                if let Some(line) = self.transfer.update(uploaded, total) {
                    self.output.progress(&line);
                }
            }
            UpdateProgress::Verifying => self.output.stage("verifying", "Verifying", Some("Verified by the Ark")),
            UpdateProgress::Installing => self.output.stage("installing", "Installing", Some("Installed")),
        }
    }
}

/// Waits for the Ark to reboot and return running `target`, recording what it
/// finds in `value`.
///
/// The Ark answers the install request before it reboots, so the old session
/// has to end first, even when the same build is reinstalled. The reboot window
/// and the pauses between attempts run on the old connection's clock.
fn verify_reboot(
    context: &Context,
    connection: &Connection,
    target: &str,
    value: &mut Value,
) -> Result<(), Error> {
    // Poll the old session every 250ms until it ends, within the reboot window
    let clock = connection.client.clock();
    let deadline = restart(&context.output, &clock);
    loop {
        match connection.client.call(
            schema::DeviceInfoRequest {},
            context.timing().with_deadline(deadline),
        ) {
            Ok(_) => clock.sleep(Duration::from_millis(250)),
            Err(darkbio_connect::Error::Closed | darkbio_connect::Error::Disconnected(_)) => break,
            Err(error) => return Err(error.into()),
        }
        if clock.now() >= deadline {
            return Err(reboot_timeout());
        }
    }

    // Rediscover the Ark every 500ms while a handshake still fits in the
    // window
    connection.ark.close();
    let device = &connection.device;
    let key = connection.identity.key();
    while deadline.saturating_duration_since(clock.now())
        > darkbio_connect::wire::transport::DEFAULT_HANDSHAKE_TIMEOUT
    {
        let found = darkbio_connect::list();
        // Discovery labels narrow the search but do not prove identity. Only a
        // new handshake with the original key can verify the returning Ark.
        for candidate in found.devices.iter().filter(|candidate| {
            if let Some(serial) = device.serial() {
                candidate.serial() == Some(serial)
            } else {
                candidate.locator() == device.locator()
            }
        }) {
            if let Ok(returned) = context.open_until(candidate.clone(), None, Some(deadline)) {
                if returned.identity.key().to_bytes() != key.to_bytes() {
                    continue;
                }
                value["returned"] = json!(true);
                value["running"] = json!(returned.info.firmware_version);
                value["verified"] = json!(returned.info.firmware_version == target);
                context.interrupt.partial(value.clone());
                return if returned.info.firmware_version == target {
                    Ok(())
                } else {
                    Err(Error::new(
                        5,
                        "update-unverified",
                        format!(
                            "the Ark returned running {} instead of {target}",
                            returned.info.firmware_version
                        ),
                    ))
                };
            }
        }
        clock.sleep(Duration::from_millis(500));
    }

    // Report the timeout only once the whole window has passed
    clock.sleep_until(deadline);
    Err(reboot_timeout())
}

/// Starts the countdown to the 120s `ark` allows a restart, and returns that
/// deadline.
fn restart(output: &Output, clock: &Clock) -> std::time::Instant {
    let deadline = clock.now() + REBOOT_WAIT;
    output.wait(
        "restarting",
        "Waiting for the Ark to restart",
        crate::output::Wait::Deadline(deadline),
    );
    deadline
}

/// Builds the error for an Ark that did not return within the reboot window.
fn reboot_timeout() -> Error {
    Error::new(7, "timeout", "the Ark did not return within 120 seconds")
}

/// Expected device approval method, used for guidance rather than authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Approval {
    /// No phone or button approval, as for an unpaired Ark.
    None,
    /// Approval on the owner's phone in Ark Companion, as a paired, unlocked
    /// Ark asks for.
    Phone,
    /// Approval by a press of the Ark's button, as a paired, locked Ark asks
    /// for.
    Button,
}

/// Predicts the approval an update needs from the Ark's pairing and lock
/// state, leaving it unknown on firmware this CLI does not support.
///
/// Older firmware omits these flags. An absent flag cannot mean "unpaired" on
/// the recovery path, so the approval stays unknown and the Ark handles it.
fn approval(info: &schema::DeviceInfoResponse) -> Option<Approval> {
    check_compatibility(info).ok()?;
    Some(if !info.paired {
        Approval::None
    } else if info.unlocked {
        Approval::Phone
    } else {
        Approval::Button
    })
}

/// Selects the published build `requested` names, or the newest candidate
/// without one.
///
/// Explicit selection permits reinstallation or downgrade requests; the Ark decides.
fn select<'a>(
    firmwares: &'a [Package],
    requested: Option<&str>,
    installed: &str,
) -> Result<Option<&'a Package>, Error> {
    // A requested build must be a valid version the listing publishes
    if let Some(version) = requested {
        package::Version::parse(version)
            .map_err(|error| error.hint("pick a version from `ark firmware list`"))?;
        return firmwares
            .iter()
            .find(|firmware| firmware.version == version)
            .map(Some)
            .ok_or_else(|| {
                Error::new(
                    1,
                    "invalid-version",
                    format!("firmware {version} is not published for this environment"),
                )
                .hint("pick a version from `ark firmware list`")
            });
    }

    // Otherwise the newest candidate wins, since the listing is newest first
    for firmware in firmwares {
        if package::candidate(firmware, installed)? {
            return Ok(Some(firmware));
        }
    }
    Ok(None)
}

/// Shows the candidates and the installed build, adding the installed one when
/// the listing lacks it.
fn listing(output: &Output, installed: &str, firmwares: &[Package]) -> Result<(), Error> {
    // Keep the candidates and the installed build from the listing
    let mut rows = Vec::new();
    for firmware in firmwares {
        let candidate = package::candidate(firmware, installed)?;
        if candidate || firmware.version == installed {
            rows.push(json!({
                "version": firmware.version,
                "published": firmware.published,
                "size_bytes": firmware.size,
                "sha256": hex::encode(firmware.sha256),
                "summary": firmware.summary,
                "installed": firmware.version == installed,
                "candidate": candidate,
            }));
        }
    }

    // An installed build missing from the listing still gets a row, first
    if !rows.iter().any(|row| row["installed"] == true) {
        rows.insert(
            0,
            json!({"version":installed,"published":null,"size_bytes":null,"sha256":null,
            "summary":null,"installed":true,"candidate":false}),
        );
    }

    // JSON keeps the whole release notes, in the listing's order
    let update = select(firmwares, None, installed)?.map(|firmware| firmware.version.clone());
    let document = json!({"installed":installed,"update":update,"firmwares":rows});
    output.document_with(&document, |theme| listing_table(theme, rows))
}

/// Renders the builds newest first, with each candidate's whole headline below
/// the table.
fn listing_table(theme: &Theme, mut rows: Vec<Value>) -> String {
    // Sort the installed build in among the candidates, even when the listing
    // lacks it
    rows.sort_by_cached_key(|row| {
        std::cmp::Reverse(package::Version::parse(row["version"].as_str().unwrap_or("")).ok())
    });
    for row in &mut rows {
        row["status"] = json!(if row["candidate"] == true {
            "update"
        } else {
            "installed"
        });
    }

    // Print the headlines under the table, since a column would cut them
    let mut sections = vec![human::table(
        theme,
        &rows,
        &[
            ("VERSION", "version"),
            ("PUBLISHED", "published"),
            ("SIZE", "size_bytes"),
            ("STATUS", "status"),
        ],
        None,
    )];
    for row in &rows {
        if row["candidate"] == true {
            let headline = package::headline(row["summary"].as_str().unwrap_or(""));
            sections.push(human::block(
                theme,
                row["version"].as_str(),
                &[(String::new(), style::printable(headline))],
            ));
        }
    }
    sections.join("\n\n")
}

/// Environment-specific package client and optional Access credentials.
pub(crate) struct Packages {
    /// Connection's clock, which bounds the Access login helpers.
    clock: Clock,
    /// HTTPS downloader retaining connections and refusing automatic redirects.
    agent: ureq::Agent,
    /// Selected package origin; credentials are sent only to this host.
    origin: &'static str,
    /// Cached Access application token for a protected package host.
    token: Option<String>,
}

impl Packages {
    /// Selects the package host and tries cached credentials without prompting
    /// for login.
    pub fn new(context: &Context, clock: Clock, env: Option<Environment>) -> Result<Self, Error> {
        let origin = match env.ok_or_else(|| {
            Error::new(4, "environment-unknown", "cloud environment unknown")
                .hint("select one with --env")
        })? {
            Environment::Release => "https://pkg.dark.bio",
            Environment::Staging => "https://pkg.darkbio.xyz",
            Environment::Develop => "https://pkg.darkbio.dev",
        };
        let token = if origin != "https://pkg.dark.bio" {
            access::cached(context, &clock, origin)
        } else {
            None
        };
        let result = Self {
            clock,
            agent: http::agent(Duration::from_secs(context.options.timeout), 0),
            origin,
            token,
        };
        Ok(result)
    }

    /// Fetches a size-bounded listing and validates every artifact before selection.
    pub fn list(&mut self, context: &Context) -> Result<Vec<Package>, Error> {
        let mut response = self.get(context, "imgs/arkos.pkgs")?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec()
            .map_err(http::error)?;
        serde_json::from_slice::<package::Listing>(&bytes)
            .map_err(|error| {
                Error::new(
                    4,
                    "cloud-unreachable",
                    format!("invalid package listing: {error}"),
                )
            })?
            .firmwares()
    }

    /// Fetches one path and retries once after a recognized Access login challenge.
    ///
    /// An unrelated redirect never changes the destination or receives credentials.
    fn get(
        &mut self,
        context: &Context,
        path: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        // Send any token as a sensitive header, and log in once after a
        // challenge before retrying
        let fetch = |token: Option<&str>| {
            let mut request = self.agent.get(format!("{}/{path}", self.origin));
            if let Some(token) = token {
                let mut value = ureq::http::HeaderValue::from_str(token)
                    .map_err(|_| Error::new(4, "login-required", "invalid package credentials"))?;
                value.set_sensitive(true);
                request = request.header("cf-access-token", value);
            }
            request.call().map_err(http::error)
        };
        let response = fetch(self.token.as_deref())?;
        let response = if access::required(self.origin, response.status(), response.headers()) {
            self.token = Some(access::authenticate(context, &self.clock, self.origin)?);
            fetch(self.token.as_deref())?
        } else {
            response
        };

        // A second challenge fails with the manual login command, and any other
        // status but success is a cloud failure
        if access::required(self.origin, response.status(), response.headers()) {
            return Err(Error::new(
                4,
                "login-required",
                "package access still refused after login",
            )
            .hint(format!(
                "run `cloudflared access login --app {}`",
                self.origin
            )));
        }
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(Error::new(
                4,
                "cloud-unreachable",
                format!("package host returned HTTP {}", response.status()),
            ))
        }
    }
}

/// Lazy archive reader opened only after the Ark accepts update preparation.
struct Download<'a> {
    /// Package client retained for the first read and any Access challenge.
    packages: &'a mut Packages,
    /// CLI timing and input policy for opening the archive request.
    context: &'a Context,
    /// Validated archive path relative to the selected package origin.
    path: String,
    /// HTTP body retained after the first read starts the download.
    response: Option<ureq::http::Response<ureq::Body>>,
}

impl std::io::Read for Download<'_> {
    /// Opens the download on the first read and streams it, carrying CLI
    /// errors such as a login refusal through connect's reader API.
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.response.is_none() {
            self.response = Some(
                self.packages
                    .get(self.context, &self.path)
                    .map_err(std::io::Error::other)?,
            );
        }
        self.response
            .as_mut()
            .expect("download opened")
            .body_mut()
            .as_reader()
            .read(buffer)
            .map_err(http::normalize_read_error)
    }
}

/// Tests of firmware selection and listing, the compatibility gate, the
/// approval guess and the update's narration.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use clap::Parser;
    use std::io::Write;

    /// Each kind of approval leads into the same sending and install stages.
    #[test]
    fn test_update_narration() {
        for captured in
            crate::testing::narration("firmware::tests::test_update_narration", |output, clock| {
                for approval in [
                    Some(Approval::Phone),
                    Some(Approval::Button),
                    None,
                    Some(Approval::None),
                ] {
                    let mut narration = Narration::new(&output, clock.clock(), 35756441, approval);
                    narration.update(UpdateProgress::Preparing);
                    output.finish();
                    clock.advance(Duration::from_secs(11));
                    narration.update(UpdateProgress::Uploading {
                        uploaded: 0,
                        total: 35756441,
                    });
                    output.finish();
                    clock.advance(Duration::from_secs(9));
                    narration.update(UpdateProgress::Uploading {
                        uploaded: 35756441,
                        total: 35756441,
                    });
                    narration.update(UpdateProgress::Verifying);
                    output.finish();
                    clock.advance(Duration::from_secs(4));
                    narration.update(UpdateProgress::Installing);
                    output.finish();
                    clock.advance(Duration::from_secs(6));
                    output.end_stage(None, None);
                }
            })
        {
            captured.contains(&[
                "Approve the update on your phone, in Ark Companion", "Approved on your phone · 11s",
                "Sending", "Sent 34.1 MiB · 9s", "Verifying", "Verified by the Ark · 4s", "Installing", "Installed · 6s",
                "Approve the update on the Ark. Press its button with the pin that came with it, in the pinhole under the bottom right LED", "Approved with the Ark's button · 11s",
                "Approve the update if the Ark asks, on your phone or with its button", "Approved · 11s",
            ]);
            captured.approvals(&[
                ("update", Some("phone")),
                ("update", Some("button")),
                ("update", None),
            ]);
            assert!(!captured.stderr.contains("Uploaded"));
            assert!(!captured.stderr.contains("restart"));
        }
    }

    /// The restart line counts down the same 120s the wait allows.
    #[test]
    fn test_restart_countdown() {
        for captured in crate::testing::narration(
            "firmware::tests::test_restart_countdown",
            |output, clock| {
                let start = clock.clock().now();
                assert_eq!(
                    restart(&output, &clock.clock()),
                    start + Duration::from_secs(120)
                );
                output.finish();
                clock.advance(Duration::from_secs(34));
                output.redraw();
                output.end_stage(Some("Restarted on 0.12.0-74d557e"), None);
            },
        ) {
            captured.contains(&[
                "Waiting for the Ark to restart · 2m 00s left",
                "Waiting for the Ark to restart · 1m 26s left",
                "Restarted on 0.12.0-74d557e · 34s",
            ]);
            if captured.json {
                assert_eq!(captured.events()[1]["remaining_seconds"], 86);
            }
        }
    }
    /// Builds invented releases, two candidates and the installed build.
    fn releases() -> Vec<Package> {
        [
            (
                "2.10.0-ccccccc",
                "2026-10-11T01:02:03Z",
                "This example release adds support for a new sample format and improves how \
                 apps read it. Note, apps built for the previous format need a rebuild before \
                 they can run on this release.\n\nFurther release details.",
                35_651_584,
            ),
            (
                "2.9.0-bbbbbbb",
                "2026-09-16T01:02:03Z",
                "This earlier example release improves sample imports.\n\nEarlier release details.",
                35_546_727,
            ),
            (
                "2.8.0-aaaaaaa",
                "2026-08-01T01:02:03Z",
                "Installed headline that stays out of the reading view.",
                34_603_008,
            ),
        ]
        .into_iter()
        .map(|(version, published, summary, size)| Package {
            version: version.into(),
            published: published.into(),
            summary: summary.into(),
            size,
            sha256: [42; 32],
        })
        .collect()
    }

    /// A version names the build to install, and without one the newest
    /// candidate installs, or none when the Ark already runs it.
    #[test]
    fn test_firmware_version_argument_selects_a_build() {
        let firmwares = releases();
        for (version, installed, expected) in [
            (None, "2.8.0-aaaaaaa", Some("2.10.0-ccccccc")),
            (None, "2.10.0-ccccccc", None),
            (
                Some("2.9.0-bbbbbbb"),
                "2.8.0-aaaaaaa",
                Some("2.9.0-bbbbbbb"),
            ),
            (
                Some("2.8.0-aaaaaaa"),
                "2.8.0-aaaaaaa",
                Some("2.8.0-aaaaaaa"),
            ),
            (
                Some("2.8.0-aaaaaaa"),
                "2.10.0-ccccccc",
                Some("2.8.0-aaaaaaa"),
            ),
        ] {
            // Parse the command line, then select from the listing
            let cli = args::Cli::try_parse_from(
                ["ark", "firmware", "update", "--dry-run"]
                    .into_iter()
                    .chain(version),
            )
            .unwrap();
            cli.validate().unwrap();
            let Some(args::Command::Firmware(args::Firmware::Update { version, .. })) = cli.command
            else {
                panic!("expected firmware update");
            };
            let selected = select(&firmwares, version.as_deref(), installed).unwrap();
            assert_eq!(
                selected.map(|package| package.version.as_str()),
                expected,
                "{version:?}, {installed}"
            );
        }
    }

    /// An unpublished or malformed version fails with `invalid-version`,
    /// pointing at the list.
    #[test]
    fn test_firmware_version_argument_rejects_unknown_builds() {
        let firmwares = releases();
        for (version, message) in [
            (
                "2.9.0-0000000",
                "firmware 2.9.0-0000000 is not published for this environment",
            ),
            ("2.9.0", "invalid firmware version \"2.9.0\""),
        ] {
            let cli = args::Cli::try_parse_from(["ark", "firmware", "update", version]).unwrap();
            let Some(args::Command::Firmware(args::Firmware::Update { version, .. })) = cli.command
            else {
                panic!("expected firmware update");
            };
            let error = select(&firmwares, version.as_deref(), "2.8.0-aaaaaaa").unwrap_err();
            assert_eq!(
                (error.class, error.code),
                (1, "invalid-version"),
                "{version:?}"
            );
            assert_eq!(error.message, message, "{version:?}");
            assert_eq!(
                error.hints,
                ["pick a version from `ark firmware list`"],
                "{version:?}"
            );
        }
    }

    /// The installed build stays a row when the catalog is empty or holds only
    /// older releases.
    #[test]
    fn test_firmware_listing_keeps_installed_without_candidates() {
        for captured in crate::testing::narration(
            "firmware::tests::test_firmware_listing_keeps_installed_without_candidates",
            |output, clock| {
                // Each listing gets its own output, since an output writes one result
                let older = Output::test(output.json(), output.terminal(), clock.clock());
                for (catalog, output, firmwares) in
                    [("empty", output, Vec::new()), ("older", older, releases())]
                {
                    let result = listing(&output, "2.11.0-ddddddd", &firmwares);
                    assert!(result.is_ok(), "{catalog}: {result:?}");
                }
            },
        ) {
            assert_eq!(captured.stderr, "", "neither catalog emits events");

            // Both catalogs leave the installed row alone, with no update
            if captured.json {
                let documents = serde_json::Deserializer::from_str(&captured.stdout)
                    .into_iter::<Value>()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(documents.len(), 2, "one document per catalog");
                for (catalog, document) in ["empty", "older"].into_iter().zip(documents) {
                    assert_eq!(
                        document,
                        json!({
                            "installed":"2.11.0-ddddddd", "update":null,
                            "firmwares":[{
                                "version":"2.11.0-ddddddd", "published":null,
                                "size_bytes":null, "sha256":null, "summary":null,
                                "installed":true, "candidate":false,
                            }],
                        }),
                        "{catalog}",
                    );
                }
            } else {
                let rows: Vec<_> = captured
                    .stdout
                    .lines()
                    .map(|line| line.split_whitespace().collect::<Vec<_>>())
                    .collect();
                assert_eq!(rows.len(), 4, "one header and installed row per catalog");
                for (catalog, rows) in ["empty", "older"].into_iter().zip(rows.as_chunks::<2>().0) {
                    assert_eq!(
                        rows,
                        &[
                            vec!["VERSION", "PUBLISHED", "SIZE", "STATUS"],
                            vec!["2.11.0-ddddddd", "-", "-", "installed"],
                        ],
                        "{catalog}",
                    );
                }
            }
        }
    }

    /// The list keeps JSON whole, and prints compact rows with the headlines
    /// whole below them.
    #[test]
    fn test_firmware_listing_columns_order_and_headlines() {
        // As the child, print the list for an installed build, listed or not
        if let Ok(installed) = std::env::var("ARK_TEST_FIRMWARE_INSTALLED") {
            let mut options = args::Cli::parse_from(["ark"]).options;
            options.json = std::env::var("ARK_TEST_FIRMWARE_JSON").unwrap() == "true";
            writeln!(std::io::stdout(), "<result>").unwrap();
            listing(&Output::new(&options), &installed, &releases()).unwrap();
            writeln!(std::io::stdout(), "</result>").unwrap();
            return;
        }

        // Both installed builds get the same candidates and headlines
        for installed in ["2.8.0-aaaaaaa", "2.8.0-develop"] {
            for json in [false, true] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "firmware::tests::test_firmware_listing_columns_order_and_headlines",
                        "--nocapture",
                    ])
                    .env("ARK_TEST_FIRMWARE_INSTALLED", installed)
                    .env("ARK_TEST_FIRMWARE_JSON", json.to_string())
                    .env("NO_COLOR", "1")
                    .env("LC_ALL", "en_US.UTF-8")
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{installed}, {json}: {output:?}");
                assert!(output.stderr.is_empty());
                let stdout = String::from_utf8(output.stdout).unwrap();
                let rendered = stdout
                    .split_once("<result>\n")
                    .unwrap()
                    .1
                    .split_once("</result>\n")
                    .unwrap()
                    .0;
                if !json {
                    let (table, headlines) = rendered.split_once("\n\n").unwrap();
                    let rows: Vec<_> = table.lines().collect();
                    assert_eq!(
                        rows[0].split_whitespace().collect::<Vec<_>>(),
                        ["VERSION", "PUBLISHED", "SIZE", "STATUS"]
                    );
                    assert_eq!(
                        rows.len(),
                        if installed.ends_with("develop") { 5 } else { 4 }
                    );
                    assert!(rows[1].starts_with("2.10.0-ccccccc  "));
                    assert!(rows[1].ends_with("34.0 MiB  update"));
                    assert!(rows[2].starts_with("2.9.0-bbbbbbb   "));
                    assert!(rows[2].ends_with("33.9 MiB  update"));
                    assert!(rows.last().unwrap().starts_with(installed));
                    assert!(rows.last().unwrap().ends_with("installed"));
                    assert!(
                        rendered
                            .lines()
                            .all(|line| console::measure_text_width(line) <= 80)
                    );
                    assert!(!rendered.ends_with("\n\n"));
                    assert!(!headlines.contains("Further release details"));
                    assert!(!headlines.contains("Earlier release details"));
                    assert!(!headlines.contains("\n2.8.0-develop\n"));
                    let sections: Vec<_> = headlines.trim_end().split("\n\n").collect();
                    let candidates = if installed.ends_with("develop") { 3 } else { 2 };
                    assert_eq!(sections.len(), candidates);
                    for (section, release) in sections.iter().zip(releases()) {
                        let (version, headline) = section.split_once('\n').unwrap();
                        assert_eq!(version, release.version);
                        assert!(headline.lines().all(|line| line.starts_with("  ")));
                        let expected = release
                            .summary
                            .split_once("\n\n")
                            .map_or(release.summary.as_str(), |(headline, _)| headline);
                        assert_eq!(
                            headline.split_whitespace().collect::<Vec<_>>().join(" "),
                            expected
                        );
                    }
                    continue;
                }

                // JSON keeps the whole notes, exact sizes, hashes, flags and order
                let document: Value = serde_json::from_str(rendered).unwrap();
                assert_eq!(document.as_object().unwrap().len(), 3);
                assert_eq!(document["installed"], installed);
                assert_eq!(document["update"], "2.10.0-ccccccc");
                let rows = document["firmwares"].as_array().unwrap();
                let offset = usize::from(installed.ends_with("develop"));
                assert_eq!(rows.len(), 3 + offset);
                if offset == 1 {
                    assert_eq!(
                        rows[0],
                        json!({
                            "version":"2.8.0-develop", "published":null, "size_bytes":null,
                            "sha256":null, "summary":null, "installed":true, "candidate":false,
                        })
                    );
                }
                for (row, release) in rows[offset..].iter().zip(releases()) {
                    assert_eq!(
                        row,
                        &json!({
                            "version":release.version, "published":release.published,
                            "size_bytes":release.size,
                            "sha256":"2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a",
                            "summary":release.summary, "installed":release.version == installed,
                            "candidate":release.version != installed,
                        })
                    );
                }

                // On a terminal dates are muted, versions title the headlines and
                // statuses stay plain
                for width in [32, 80] {
                    let theme = Theme::test(width, Color::True, true);
                    let rendered = listing_table(&theme, rows.clone());
                    assert!(
                        rendered
                            .lines()
                            .all(|line| console::measure_text_width(line) <= width)
                    );
                    assert!(rendered.contains("\n\n\x1b[1m2.10.0-ccccccc\x1b[0m\n  "));
                    if width == 80 {
                        let row = rendered.lines().nth(1).unwrap();
                        let plain = console::strip_ansi_codes(row);
                        let date = plain.split_whitespace().nth(1).unwrap();
                        assert_eq!(date.len(), 10);
                        assert!(row.contains(&format!("\x1b[38;2;124;128;152m{date}\x1b[0m")));
                        assert!(row.ends_with("34.0 MiB  update"));
                    }
                }
            }
        }
    }

    /// A newer build of the installed develop version reads `update` and prints
    /// its headline.
    #[test]
    fn test_installed_candidate_shows_update_and_headline() {
        let theme = Theme::test(80, Color::True, true);
        let rendered = listing_table(
            &theme,
            vec![json!({
                "version":"2.10.0-develop", "published":null, "size_bytes":35_651_584,
                "summary":"Apps need a rebuild.\n\nFurther release notes.", "installed":true, "candidate":true,
            })],
        );
        assert!(
            rendered
                .lines()
                .nth(1)
                .unwrap()
                .ends_with("34.0 MiB  update")
        );
        assert!(rendered.contains("\n\n\x1b[1m2.10.0-develop\x1b[0m\n  Apps need a rebuild."));
        assert!(!rendered.contains("Further release notes"));
    }

    /// Builds device info reporting firmware `version`, published at `publish`
    /// in Unix seconds.
    fn info(version: &str, publish: u64) -> schema::DeviceInfoResponse {
        schema::DeviceInfoResponse {
            firmware_version: version.into(),
            firmware_publish: publish,
            ..Default::default()
        }
    }

    /// Builds from the minimum version up pass the gate, and older or malformed
    /// versions fail it.
    #[test]
    fn test_compatibility_requires_the_protocol_batch() {
        for version in [
            "0.14.0-develop",
            "0.14.0-c1e0a5c",
            "0.15.0-develop",
            "1.0.0-0000000",
        ] {
            assert!(
                check_compatibility(&info(version, 1_793_267_015)).is_ok(),
                "{version}"
            );
        }
        for version in [
            "0.13.2-develop",
            "0.13.1-36da538",
            "0.13.1-develop",
            "0.13.0-edd6482",
            "0.12.0-74d557e",
            "0.11.7-abcdef0",
            "0.10.99-abcdef0",
            "unknown",
            "0.14.0",
        ] {
            let error = check_compatibility(&info(version, 1_793_267_016)).unwrap_err();
            assert_eq!(error.code, "firmware-outdated", "{version}");
            assert_eq!(error.class, 5);
        }
    }

    /// Develop builds need the publish cutoff, and tagged builds pass without
    /// it.
    #[test]
    fn test_develop_builds_need_the_cutoff_but_tagged_builds_do_not() {
        for version in ["0.14.0-develop", "0.15.0-develop"] {
            for publish in [0, 1_793_267_014] {
                let error = check_compatibility(&info(version, publish)).unwrap_err();
                assert_eq!(error.code, "firmware-outdated");
                assert_eq!(error.class, 5);
                assert_eq!(
                    error.message,
                    "develop build is outdated; rebuild or update the firmware"
                );
            }
            for publish in [1_793_267_015, 1_793_267_016] {
                assert!(check_compatibility(&info(version, publish)).is_ok());
            }
        }
        for version in ["0.14.0-c1e0a5c", "0.15.0-abcdef0"] {
            assert!(check_compatibility(&info(version, 0)).is_ok());
        }
    }

    /// The predicted approval follows the pairing and lock state, but only on
    /// supported firmware.
    #[test]
    fn test_approval_uses_device_state_only_on_supported_firmware() {
        for (paired, unlocked, expected) in [
            (false, false, Approval::None),
            (true, false, Approval::Button),
            (true, true, Approval::Phone),
        ] {
            let mut device = info("0.14.0-develop", 1_793_267_015);
            device.paired = paired;
            device.unlocked = unlocked;
            assert_eq!(approval(&device), Some(expected));
            device.firmware_publish -= 1;
            assert_eq!(approval(&device), None);
            device.firmware_version = "0.13.1-36da538".into();
            assert_eq!(approval(&device), None);
        }
    }

    /// Checks the gate against published image stamps. The v0.13.1 release and
    /// the last develop build before v0.14.0 fail the version floor, and the
    /// v0.14.0 release passes, as does a develop build stamped with it.
    #[test]
    fn test_published_images_straddle_the_cutoff() {
        for (version, publish, message) in [
            (
                "0.13.1-36da538",
                1_792_446_096,
                "firmware 0.13.1-36da538 requires an update; minimum 0.14.0",
            ),
            (
                "0.13.2-develop",
                1_793_140_807,
                "firmware 0.13.2-develop requires an update; minimum 0.14.0",
            ),
        ] {
            let error = check_compatibility(&info(version, publish)).unwrap_err();
            assert_eq!(
                (error.class, error.code),
                (5, "firmware-outdated"),
                "{version}/{publish}"
            );
            assert_eq!(error.message, message, "{version}/{publish}");
        }
        for version in ["0.14.0-c1e0a5c", "0.14.0-develop"] {
            // The v0.14.0 release's boot image stamp
            assert!(
                check_compatibility(&info(version, 1_793_267_015)).is_ok(),
                "{version}"
            );
        }
    }
}
