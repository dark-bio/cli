// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Device state and identity commands.

use crate::{
    args,
    context::{Connection, Context},
    error::Error,
    output::{Output, human},
    style::{Role, Theme},
};
use darkbio_connect::{DeviceKind, Identity, schema};
use serde_json::{Value, json};

/// Lists discovery metadata without opening devices, retaining useful partial
/// results.
pub(crate) fn devices(context: &Context) -> Result<(), Error> {
    // Nothing found fails only when some discovery source failed too
    let found = context.discover();
    if found.devices.is_empty() && !found.errors.is_empty() {
        return Err(Error::new(
            3,
            "no-device",
            "no Arks found; discovery was incomplete",
        ));
    }

    // Each row carries discovery metadata alone, since no device is opened
    let rows: Vec<_> = found.devices.iter().map(|device| json!({
        "locator":device.locator().to_string(), "kind":match device.kind() { DeviceKind::Hardware=>"hardware", DeviceKind::Emulator=>"emulator" },
        "name":device.name(),"serial":device.serial(),"image":device.image(),"environment":device.env(),"ready":device.ready(),
    })).collect();

    // The `environment` and `ready` columns are launcher metadata, which
    // discovery leaves unset for hardware. They show only when a row has them,
    // since a dash under READY reads as "not ready" rather than "not asked".
    let mut columns = vec![
        ("LOCATOR", "locator"),
        ("NAME", "name"),
        ("SERIAL", "serial"),
        ("KIND", "kind"),
    ];
    for (label, key) in [("ENVIRONMENT", "environment"), ("READY", "ready")] {
        if rows.iter().any(|row| !row[key].is_null()) {
            columns.push((label, key));
        }
    }

    // Print the table, pointing at --device when several Arks answer
    context
        .output
        .table(&json!({"devices":rows}), &rows, &columns)?;
    if rows.len() > 1 {
        context
            .output
            .event("hint", "select one with --device LOCATOR");
    }
    Ok(())
}

/// Prints the Ark's status, then fails on outdated firmware or hints at the
/// next step.
///
/// The status comes first, so an outdated Ark still shows its state.
pub(crate) fn status(context: &Context, recovery: args::Recovery) -> Result<(), Error> {
    let connection = context.connect_recovery(recovery.pubkey.as_deref())?;
    let value = status_value(&connection);
    print_status(&context.output, &value)?;
    connection.require_current()?;
    status_hints(context, &connection, &value);
    Ok(())
}

/// Combines authenticated identity with reported hardware and firmware snapshots.
///
/// Sync, pairing and lock state stay unknown on firmware this tool does not
/// support. The environment comes from the attestation alone, since routing
/// overrides do not become attested environment labels.
fn status_value(connection: &Connection) -> Value {
    let current = connection.require_current().is_ok();
    let info = &connection.info;
    let clock = connection.client.clock();

    // Serial, realm and model come only from an attestation, which also flags a
    // reported hardware version that differs from the attested one
    let reported = format!("{} - {}", info.version_str, info.revision_str);
    let (trust, serial, realm, model, mismatch, env) = match &connection.identity {
        Identity::Attested { env, device } => (
            "attested",
            json!(device.serial),
            json!(match device.realm {
                darkbio_connect::trust::Realm::Hardware => "hardware",
                darkbio_connect::trust::Realm::Emulator => "emulator",
            }),
            json!(
                String::from_utf8(device.model.clone())
                    .unwrap_or_else(|_| format!("0x{}", hex::encode(&device.model)))
            ),
            (device.version != reported).then(|| device.version.clone()),
            Some(env.to_string()),
        ),
        Identity::SelfSigned(_) => (
            "self-signed",
            Value::Null,
            Value::Null,
            Value::Null,
            None,
            None,
        ),
        Identity::Recovered(_) => ("pinned", Value::Null, Value::Null, Value::Null, None, None),
    };

    // Assemble the document, leaving state that needs supported firmware null
    json!({"name":connection.device.name(),"serial":serial,
        "hardware":{"version":info.version_str,"revision":info.revision_str,"model":model},
        "firmware":{"version":info.firmware_version,"published":timestamp(info.firmware_publish)},
        "trust":trust,"environment":env,"realm":realm,"synced":current.then(|| darkbio_connect::cloud_synced(info, &clock)),"paired":current.then_some(info.paired),"unlocked":current.then_some(info.unlocked),
        "identity":hex::encode(connection.identity.key().fingerprint().to_bytes()),
        "pubkey":hex::encode(connection.identity.key().to_bytes()),"mismatch":mismatch})
}

/// Prints a status document, as the compact block for people and whole in JSON.
fn print_status(output: &Output, value: &Value) -> Result<(), Error> {
    output.document_with(value, |theme| status_block(theme, value))
}

/// Groups identifying facts and readiness under the Ark's name.
fn status_block(theme: &Theme, value: &Value) -> String {
    let field = |key: &str| human::value(theme, key, &value[key]);
    let flag = |key: &str, yes: &str, no: &str, role: Role| match value[key].as_bool() {
        Some(true) => theme.mark(Role::Success, yes),
        Some(false) => theme.mark(role, no),
        None => theme.paint(Role::Muted, "-"),
    };
    let hardware = &value["hardware"];

    // Name the Ark by its serial and builds, leaving keys and fingerprints to JSON
    let mut rows = vec![
        (
            "Serial".into(),
            if value["serial"].is_null() {
                theme.mark(Role::Muted, "unverified")
            } else {
                field("serial")
            },
        ),
        (
            "Hardware".into(),
            format!(
                "{}, revision {}",
                crate::output::scalar(&hardware["version"]),
                crate::output::scalar(&hardware["revision"])
            ),
        ),
        (
            "Firmware".into(),
            crate::output::scalar(&value["firmware"]["version"]),
        ),
        ("Environment".into(), field("environment")),
    ];
    if !value["mismatch"].is_null() {
        rows.push(("Mismatch".into(), field("mismatch")));
    }
    if value.get("enrolled").is_some() {
        rows.push(("Enrolled".into(), field("enrolled")));
    }

    // Readiness follows as its own group, where not synced is neutral since it
    // needs nobody
    rows.extend([
        (String::new(), String::new()),
        ("Trust".into(), field("trust")),
        (
            "Cloud".into(),
            flag("synced", "synced", "not synced", Role::Muted),
        ),
        (
            "Pairing".into(),
            flag("paired", "paired", "not paired", Role::Attention),
        ),
        (
            "Lock".into(),
            flag("unlocked", "unlocked", "locked", Role::Attention),
        ),
    ]);
    human::block(theme, Some(value["name"].as_str().unwrap_or("Ark")), &rows)
}

/// Suggests the next enrollment, pairing or unlock action from the reported state.
fn status_hints(context: &Context, connection: &Connection, value: &Value) {
    if matches!(connection.identity, Identity::SelfSigned(_))
        && connection.device.kind() == DeviceKind::Emulator
    {
        context
            .output
            .event("hint", "run `ark enroll` to get an attested identity");
    }
    if value["paired"] == false {
        context.output.event("hint", "run `ark pair`");
    } else if value["unlocked"] == false {
        context.output.event("hint", "run `ark unlock`");
    }
}

/// Unlocks a paired Ark when needed and reports whether this invocation changed it.
pub(crate) fn unlock(context: &Context) -> Result<(), Error> {
    let connection = context.connect(None)?;
    let state = &connection.info;
    if !state.paired {
        return Err(Error::new(5, "not-paired", "the Ark is not paired").hint("run `ark pair`"));
    }
    if state.unlocked {
        context.output.event("note", "already unlocked");
    } else {
        context.unlock(&connection)?;
    }
    context
        .output
        .document(&json!({"unlocked":true,"changed":!state.unlocked}))
}

/// Syncs with the cloud, then prints the Ark's registration and fails when it
/// is inactive.
pub(crate) fn genuine(context: &Context) -> Result<(), Error> {
    // Refresh the cloud keys and signed clock before asking the registry
    let connection = context.connect(None)?;
    connection.client.sync(context.timing())?;

    // A refused proof from a self-signed emulator points at enrollment
    let registration = connection
        .client
        .genuine(context.timing())
        .map_err(|error| {
            let mut error = Error::from(error);
            if error.code == "proof-rejected"
                && matches!(connection.identity, Identity::SelfSigned(_))
                && connection.device.kind() == DeviceKind::Emulator
            {
                error
                    .hints
                    .push("enroll the emulator with `ark enroll`".into());
            }
            error
        })?;

    // Print the registration first, so an inactive one still shows its flags
    context.output.document(&json!({"serial":registration.serial,"enrolled":timestamp(registration.enrolled.max(0) as u64),
        "active":registration.active(),"disabled":registration.disabled,"expired":registration.expired,"superseded":registration.superseded}))?;
    if !registration.active() {
        return Err(
            Error::new(4, "registry-inactive", "the Ark registration is inactive").hint(
                if registration.disabled || connection.device.kind() != DeviceKind::Emulator {
                    "contact Dark Bio"
                } else {
                    "run `ark-emulator stop`, `ark-emulator wipe` and `ark-emulator start` \
                     for a fresh device, then `ark enroll`"
                },
            ),
        );
    }
    Ok(())
}

/// Installs a supplied attestation or directs online enrollment to Ark Hub.
///
/// After installation, it reconnects without recovery pinning to verify the new
/// identity. A reconnect failure still reports that enrollment was acknowledged.
pub(crate) fn enroll(context: &Context, args: args::Enroll) -> Result<(), Error> {
    // Read a supplied attestation before connecting
    let certificate = args
        .cwt
        .as_ref()
        .map(|path| {
            context.output.event("step", "reading attestation");
            std::fs::read(path).map_err(|error| {
                Error::new(1, "file-unreadable", format!("{}: {error}", path.display()))
            })
        })
        .transpose()?;

    // Installing an attestation crosses the compatibility gate, since it may
    // enroll an old device
    let connection = if certificate.is_some() {
        context.connect_recovery(args.recovery.pubkey.as_deref())?
    } else {
        context.connect(args.recovery.pubkey.as_deref())?
    };

    // Without an attestation, an attested Ark is already enrolled, and any
    // other one is sent to Ark Hub
    let Some(certificate) = certificate else {
        if matches!(connection.identity, Identity::Attested { .. }) {
            let mut error = Error::new(
                5,
                "already-enrolled",
                "the Ark already has an attested identity",
            );
            if connection.device.kind() == DeviceKind::Emulator {
                error.hints.push(
                    "a new identity needs a fresh device, from `ark-emulator stop`, \
                     `ark-emulator wipe` and `ark-emulator start`"
                        .into(),
                );
            }
            return Err(error);
        }
        let env = connection.env.ok_or_else(|| {
            Error::new(4, "environment-unknown", "cloud environment unknown")
                .hint("select one with --env")
        })?;
        let url = hub(env);
        context
            .output
            .event("hint", format!("enroll online at {url}"));
        context
            .output
            .document(&json!({"enrolled":false,"url":url}))?;
        return Err(Error::new(
            1,
            "enrollment-required",
            "online enrollment happens at Ark Hub",
        ));
    };

    // Install the attestation, then reconnect without the pin to verify it
    connection.client.call(
        schema::OnboardingRequest {
            device_attestation: certificate,
        },
        context.timing(),
    )?;
    let device = connection.device.clone();
    drop(connection);
    match context
        .open(device, None)
        .map(|connection| status_value(&connection))
    {
        Ok(mut value) => {
            value["enrolled"] = json!(true);
            print_status(&context.output, &value)
        }
        Err(error) => {
            context
                .output
                .document(&json!({"enrolled":true,"status":null}))?;
            Err(error)
        }
    }
}

/// Formats Unix seconds as UTC RFC 3339, leaving unrepresentable dates absent.
pub(crate) fn timestamp(seconds: u64) -> Option<String> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Selects the browser enrollment origin for the chosen cloud route.
pub(crate) fn hub(env: darkbio_connect::trust::Environment) -> &'static str {
    use darkbio_connect::trust::Environment::*;
    match env {
        Release => "https://hub.dark.bio",
        Staging => "https://hub.darkbio.xyz",
        Develop => "https://hub.darkbio.dev",
    }
}

/// Tests of the human status layout.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;
    use clap::Parser;
    use std::io::Write;

    /// Builds an invented attested status with the complete JSON field set.
    fn status() -> Value {
        json!({
            "name":"Example Ark", "serial":"example-1",
            "hardware":{"version":"Ark I","revision":"0.9.0","model":"example-model"},
            "firmware":{"version":"0.12.0-74d557e","published":"2026-10-10T16:27:08Z"},
            "trust":"attested","environment":"release","realm":"hardware",
            "synced":true,"paired":true,"unlocked":true,
            "identity":"0123456789abcdef","pubkey":"abcdef0123456789","mismatch":null,
        })
    }

    /// The Ark's name titles two groups, with identifying facts before readiness.
    #[test]
    fn test_status_title_and_groups() {
        let theme = Theme::test(80, Color::Basic, true);
        assert_eq!(
            status_block(&theme, &status()),
            concat!(
                "\x1b[1mExample Ark\x1b[0m\n",
                "  Serial       example-1\n",
                "  Hardware     Ark I, revision 0.9.0\n",
                "  Firmware     0.12.0-74d557e\n",
                "  Environment  release\n\n",
                "  Trust        \x1b[1m✓\x1b[0m attested\n",
                "  Cloud        \x1b[1m✓\x1b[0m synced\n",
                "  Pairing      \x1b[1m✓\x1b[0m paired\n",
                "  Lock         \x1b[1m✓\x1b[0m unlocked",
            )
        );
    }

    /// Mismatch and enrollment rows finish the identifying group.
    #[test]
    fn test_status_extra_identity_rows_precede_readiness() {
        let theme = Theme::test(80, Color::Off, true);
        let mut value = status();
        value["mismatch"] = json!("Ark II - A");
        value["enrolled"] = json!(true);
        let rendered = status_block(&theme, &value);
        assert!(rendered.contains(concat!(
            "  Environment  release\n",
            "  Mismatch     Ark II - A\n",
            "  Enrolled     ✓ yes\n\n",
            "  Trust        ✓ attested",
        )));
        assert_eq!(rendered.matches("\n\n").count(), 1);
    }

    /// Unverified serials and unsynced cloud state are neutral; pairing needs action.
    #[test]
    fn test_status_marks_unverified_and_not_ready_states() {
        // A self-signed or pinned identity carries no attested fields
        let theme = Theme::test(80, Color::True, true);
        let mut value = status();
        value["serial"] = Value::Null;
        value["realm"] = Value::Null;
        value["environment"] = Value::Null;
        value["hardware"]["model"] = Value::Null;
        value["synced"] = json!(false);
        value["paired"] = json!(false);
        value["unlocked"] = json!(false);
        for trust in ["self-signed", "pinned"] {
            value["trust"] = json!(trust);
            let rendered = status_block(&theme, &value);
            for state in ["unverified", "not synced"] {
                assert!(
                    rendered.contains(&format!("\x1b[38;2;124;128;152m·\x1b[0m {state}")),
                    "{trust}, {state}"
                );
            }
            for state in [trust, "not paired", "locked"] {
                assert!(
                    rendered.contains(&format!("\x1b[1m\x1b[38;2;232;162;74m!\x1b[0m {state}")),
                    "{trust}, {state}"
                );
            }
        }

        // Unknown readiness stays distinct from false, including with ASCII marks
        for key in ["synced", "paired", "unlocked"] {
            value[key] = Value::Null;
        }
        let rendered = status_block(&theme, &value);
        for label in ["Cloud", "Pairing", "Lock"] {
            let row = rendered.lines().find(|line| line.contains(label)).unwrap();
            assert!(row.ends_with("\x1b[38;2;124;128;152m-\x1b[0m"), "{label}");
        }
        let plain = status_block(&Theme::test(80, Color::Off, false), &value);
        assert!(plain.contains("  Serial       - unverified\n"));
    }

    /// The status writer omits detailed identity fields only from the reading view.
    #[test]
    fn test_status_json_preserves_details() {
        // Run the actual writer in a child so stdout can be captured independently
        let value = status();
        if let Ok(json) = std::env::var("ARK_TEST_STATUS_JSON") {
            let mut options = args::Cli::parse_from(["ark"]).options;
            options.json = json == "true";
            writeln!(std::io::stdout(), "<result>").unwrap();
            print_status(&Output::new(&options), &value).unwrap();
            writeln!(std::io::stdout(), "</result>").unwrap();
            return;
        }

        // JSON keeps the entire document, while the reading view omits its long fields
        for json in [false, true] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "device::tests::test_status_json_preserves_details",
                    "--nocapture",
                ])
                .env("ARK_TEST_STATUS_JSON", json.to_string())
                .env("NO_COLOR", "1")
                .env("LC_ALL", "en_US.UTF-8")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty());
            let stdout = String::from_utf8(output.stdout).unwrap();
            let rendered = stdout
                .split_once("<result>\n")
                .unwrap()
                .1
                .split_once("</result>\n")
                .unwrap()
                .0;
            if json {
                assert_eq!(serde_json::from_str::<Value>(rendered).unwrap(), value);
            } else {
                assert!(rendered.starts_with("Example Ark\n  Serial       example-1\n"));
                for omitted in [
                    "Identity",
                    "0123456789abcdef",
                    "abcdef0123456789",
                    "Realm",
                    "hardware",
                    "example-model",
                    "published",
                    "2026-10-10",
                ] {
                    assert!(!rendered.contains(omitted), "{omitted}");
                }
            }
        }
    }
}
