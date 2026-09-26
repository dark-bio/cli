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
    progress::Transfer,
};
use darkbio_clock::Clock;
use darkbio_connect::{UpdateProgress, schema, trust::Environment};
use package::Package;
use serde_json::{Value, json};
use std::time::Duration;

/// Verification window covering old-session closure and discovery after installation.
pub(crate) const REBOOT_WAIT: Duration = Duration::from_secs(120);

/// First firmware version speaking the wire protocol this CLI uses.
pub(crate) const MINIMUM_VERSION: &str = "0.11.5";

/// Bump when current-release changes require developers to rebuild their image.
pub(crate) const MINIMUM_DEVELOP_PUBLISH: u64 = 1_789_461_235; // 2026-09-15 08:33:55 UTC

/// Requires the protocol batch and, for mutable develop builds, its publish cutoff.
/// This is CLI compatibility guidance based on reported firmware metadata.
pub(crate) fn check_compatibility(info: &schema::DeviceInfoResponse) -> Result<(), Error> {
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
/// The CLI owns package retrieval, consent and reboot verification; connect owns the
/// authorized update sequence. Partial results distinguish installation from return.
pub(crate) fn run(context: &Context, command: args::Firmware) -> Result<(), Error> {
    let connection = context.connect_recovery(None)?;
    let clock = connection.client.clock();
    let mut packages = Packages::new(context, clock.clone(), connection.env)?;
    let firmwares = packages.list(context)?;
    if let args::Firmware::List = command {
        return listing(context, &connection, &firmwares);
    }
    let args::Firmware::Update {
        version,
        dry_run,
        no_wait,
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
    context.output.event(
        "note",
        format!(
            "firmware {} -> {} ({:.1} MiB): {}",
            connection.info.firmware_version,
            target.version,
            target.size as f64 / 1048576.0,
            target.summary
        ),
    );
    if !context.options.yes
        && (!context.interactive()
            || !context.confirm(
                &format!("Install {} and reboot the Ark?", target.version),
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
    let mut transfer = Transfer::new(context.output.terminal(), clock.clone());
    let result = connection.client.update_firmware(
        &target.firmware(),
        &mut reader,
        context.timing(),
        |stage| match stage {
            UpdateProgress::Preparing => match approval {
                Some(Approval::None) => {}
                Some(Approval::Phone) => context
                    .output
                    .event("approve", "firmware update (Ark Companion on your phone)"),
                Some(Approval::Button) => context
                    .output
                    .event("approve", "firmware update (press the button on the Ark)"),
                None => context.output.event(
                    "note",
                    "if requested, approve the update on your phone or press the Ark's button",
                ),
            },
            UpdateProgress::Uploading { uploaded, total } => {
                if let Some(line) = transfer.update(uploaded, total) {
                    context.output.progress(&line);
                }
            }
            UpdateProgress::Verifying => context
                .output
                .event("progress", "verifying firmware on the Ark"),
            UpdateProgress::Installing => context.output.event("progress", "installing firmware"),
        },
    );
    if let Err(error) = result {
        context.output.document(&value)?;
        let mut error: Error = error.into();
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
    value["installed"] = json!(true);
    value["running"] = Value::Null;
    context.interrupt.partial(value.clone());
    if !no_wait {
        context
            .output
            .event("progress", "waiting for the Ark to reboot");
        context.output.wait("waiting for the Ark to return", None);
        let started = clock.now();
        let result = verify_reboot(context, &connection, &target.version, &mut value);
        context.output.finish();
        if result.is_ok() {
            context.output.human_event(
                "progress",
                format!(
                    "returned after {} s; verified {}",
                    clock.elapsed(started).as_secs(),
                    target.version
                ),
            );
        }
        context.output.document(&value)?;
        return result;
    }
    context.output.document(&value)
}

/// Installation acknowledges before scheduling reboot. Observe the old session
/// ending first, including when reinstalling the same build. The reboot window
/// and the pauses between attempts run on the old connection's clock.
fn verify_reboot(
    context: &Context,
    connection: &Connection,
    target: &str,
    value: &mut Value,
) -> Result<(), Error> {
    let clock = connection.client.clock();
    let deadline = clock.now() + REBOOT_WAIT;
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
    clock.sleep_until(deadline);
    Err(reboot_timeout())
}
/// Reports that installation was not verified within the reboot window.
fn reboot_timeout() -> Error {
    Error::new(7, "timeout", "the Ark did not return within 120 seconds")
}

/// Expected device approval method, used for guidance rather than authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Approval {
    /// An unpaired Ark needs no phone or button approval.
    None,
    /// A paired, unlocked Ark asks its companion for approval.
    Phone,
    /// A paired, locked Ark asks for physical button approval.
    Button,
}

/// Older firmware omits these flags. An absent flag cannot mean "unpaired" on
/// the recovery path, so leave approval unknown and let the Ark handle it.
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
/// Finds an exact requested build or the first candidate in a newest-first listing.
/// Explicit selection permits reinstallation or downgrade requests; the Ark decides.
fn select<'a>(
    firmwares: &'a [Package],
    requested: Option<&str>,
    installed: &str,
) -> Result<Option<&'a Package>, Error> {
    if let Some(version) = requested {
        package::Version::parse(version)?;
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
            });
    }
    for firmware in firmwares {
        if package::candidate(firmware, installed)? {
            return Ok(Some(firmware));
        }
    }
    Ok(None)
}
/// Shows candidates and the installed build, even when that build is no longer published.
fn listing(context: &Context, connection: &Connection, firmwares: &[Package]) -> Result<(), Error> {
    let installed = &connection.info.firmware_version;
    let mut rows = Vec::new();
    for firmware in firmwares {
        let candidate = package::candidate(firmware, installed)?;
        if candidate || &firmware.version == installed {
            rows.push(json!({
                "version": firmware.version,
                "published": firmware.published,
                "size_bytes": firmware.size,
                "sha256": hex::encode(firmware.sha256),
                "summary": firmware.summary,
                "installed": &firmware.version == installed,
                "candidate": candidate,
            }));
        }
    }
    if !rows.iter().any(|row| row["installed"] == true) {
        rows.insert(
            0,
            json!({"version":installed,"published":null,"size_bytes":null,"sha256":null,
            "summary":null,"installed":true,"candidate":false}),
        );
    }
    let update = select(firmwares, None, installed)?.map(|firmware| firmware.version.clone());
    let document = json!({"installed":installed,"update":update,"firmwares":rows});
    for row in &mut rows {
        row["flags"] = json!(
            [
                (row["installed"] == true).then_some("installed"),
                (row["candidate"] == true).then_some("update"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        );
    }
    let groups: Vec<_> = rows
        .iter()
        .map(|row| {
            row["version"]
                .as_str()
                .unwrap_or("unknown")
                .split('-')
                .next()
                .unwrap_or("unknown")
                .to_string()
        })
        .collect();
    context.output.grouped_table(
        &document,
        &rows,
        &[
            ("VERSION", "version"),
            ("PUBLISHED", "published"),
            ("SIZE", "size_bytes"),
            ("FLAGS", "flags"),
            ("SUMMARY", "summary"),
        ],
        &groups,
    )
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
    /// Selects the package host and tries cached credentials without prompting for login.
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
    /// An unrelated redirect never changes the destination or receives credentials.
    fn get(
        &mut self,
        context: &Context,
        path: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
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
    /// Opens once and streams bytes, preserving CLI login errors through connect's reader API.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str, publish: u64) -> schema::DeviceInfoResponse {
        schema::DeviceInfoResponse {
            firmware_version: version.into(),
            firmware_publish: publish,
            ..Default::default()
        }
    }

    #[test]
    fn compatibility_requires_the_protocol_batch() {
        for version in [
            "0.11.5-develop",
            "0.11.5-abcdef0",
            "0.12.0-develop",
            "1.0.0-0000000",
        ] {
            assert!(
                check_compatibility(&info(version, MINIMUM_DEVELOP_PUBLISH)).is_ok(),
                "{version}"
            );
        }
        for version in [
            "0.11.4-develop",
            "0.11.4-abcdef0",
            "0.10.99-abcdef0",
            "unknown",
            "0.11.5",
        ] {
            let error =
                check_compatibility(&info(version, MINIMUM_DEVELOP_PUBLISH + 1)).unwrap_err();
            assert_eq!(error.code, "firmware-outdated", "{version}");
            assert_eq!(error.class, 5);
        }
    }

    #[test]
    fn develop_builds_need_the_cutoff_but_tagged_builds_do_not() {
        for version in ["0.11.5-develop", "0.12.0-develop"] {
            for publish in [0, MINIMUM_DEVELOP_PUBLISH - 1] {
                let error = check_compatibility(&info(version, publish)).unwrap_err();
                assert_eq!(error.code, "firmware-outdated");
                assert_eq!(error.class, 5);
                assert_eq!(
                    error.message,
                    "develop build is outdated; rebuild or update the firmware"
                );
            }
            for publish in [MINIMUM_DEVELOP_PUBLISH, MINIMUM_DEVELOP_PUBLISH + 1] {
                assert!(check_compatibility(&info(version, publish)).is_ok());
            }
        }
        for version in ["0.11.5-abcdef0", "0.12.0-abcdef0"] {
            assert!(check_compatibility(&info(version, 0)).is_ok());
        }
    }

    #[test]
    fn approval_uses_device_state_only_on_supported_firmware() {
        for (paired, unlocked, expected) in [
            (false, false, Approval::None),
            (true, false, Approval::Button),
            (true, true, Approval::Phone),
        ] {
            let mut device = info("0.11.5-develop", MINIMUM_DEVELOP_PUBLISH);
            device.paired = paired;
            device.unlocked = unlocked;
            assert_eq!(approval(&device), Some(expected));
            device.firmware_publish -= 1;
            assert_eq!(approval(&device), None);
            device.firmware_version = "0.11.4-abcdef0".into();
            assert_eq!(approval(&device), None);
        }
    }
}
