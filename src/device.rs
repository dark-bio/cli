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
    output::human,
    style::{Role, Theme},
};
use darkbio_connect::{DeviceKind, Identity, schema};
use serde_json::{Value, json};

/// Lists discovery metadata without opening devices, retaining useful partial results.
pub(crate) fn devices(context: &Context) -> Result<(), Error> {
    let found = context.discover();
    if found.devices.is_empty() && !found.errors.is_empty() {
        return Err(Error::new(
            3,
            "no-device",
            "no Arks found; discovery was incomplete",
        ));
    }
    let rows: Vec<_> = found.devices.iter().map(|device| json!({
        "locator":device.locator().to_string(), "kind":match device.kind() { DeviceKind::Hardware=>"hardware", DeviceKind::Emulator=>"emulator" },
        "name":device.name(),"serial":device.serial(),"image":device.image(),"environment":device.env(),"ready":device.ready(),
    })).collect();
    context.output.table(
        &json!({"devices":rows}),
        &rows,
        &[
            ("LOCATOR", "locator"),
            ("NAME", "name"),
            ("SERIAL", "serial"),
            ("KIND", "kind"),
            ("ENV", "environment"),
            ("STATE", "ready"),
        ],
    )?;
    if rows.len() > 1 {
        context
            .output
            .event("hint", "select one with --device LOCATOR");
    }
    Ok(())
}

/// Prints offline device state before reporting an outdated firmware error.
pub(crate) fn status(context: &Context, recovery: args::Recovery) -> Result<(), Error> {
    let connection = context.connect_recovery(recovery.pubkey.as_deref())?;
    let value = status_value(&connection);
    print_status(context, &value)?;
    connection.require_current()?;
    status_hints(context, &connection, &value);
    Ok(())
}

/// Combines authenticated identity with reported hardware and firmware snapshots.
/// Fields absent from older protocols stay unknown; routing overrides do not become
/// attested environment labels.
fn status_value(connection: &Connection) -> Value {
    let current = connection.require_current().is_ok();
    let info = &connection.info;
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
    json!({"name":connection.device.name(),"serial":serial,
        "hardware":{"version":info.version_str,"revision":info.revision_str,"model":model},
        "firmware":{"version":info.firmware_version,"published":timestamp(info.firmware_publish)},
        "trust":trust,"environment":env,"realm":realm,"synced":current.then(|| darkbio_connect::cloud_synced(info)),"paired":current.then_some(info.paired),"unlocked":current.then_some(info.unlocked),
        "identity":hex::encode(connection.identity.key().fingerprint().to_bytes()),
        "pubkey":hex::encode(connection.identity.key().to_bytes()),"mismatch":mismatch})
}

/// Uses the compact human status layout with the same complete machine document.
fn print_status(context: &Context, value: &Value) -> Result<(), Error> {
    context
        .output
        .document_with(value, |theme| status_block(theme, value))
}

/// Groups identity and state facts; the full public key belongs in JSON.
fn status_block(theme: &Theme, value: &Value) -> String {
    let field = |key: &str| human::value(theme, key, &value[key]);
    let flag = |key: &str, yes: &str, no: &str| match value[key].as_bool() {
        Some(true) => theme.mark(Role::Success, yes),
        Some(false) => theme.mark(Role::Attention, no),
        None => theme.paint(Role::Muted, "-"),
    };
    let hardware = &value["hardware"];
    let firmware = &value["firmware"];
    let fingerprint = value["identity"].as_str().unwrap_or("-");
    let sep = theme.separator();
    let mut rows = vec![
        (
            String::new(),
            format!(
                "{}  {}",
                theme.paint(Role::Heading, value["name"].as_str().unwrap_or("Ark")),
                theme.paint(Role::Muted, human::value(theme, "serial", &value["serial"]))
            ),
        ),
        (
            "Hardware".into(),
            format!(
                "{}, revision {}, model {}",
                crate::output::scalar(&hardware["version"]),
                crate::output::scalar(&hardware["revision"]),
                crate::output::scalar(&hardware["model"])
            ),
        ),
        (String::new(), String::new()),
        (
            "Firmware".into(),
            format!(
                "{}, published {}",
                crate::output::scalar(&firmware["version"]),
                human::value(theme, "published", &firmware["published"])
            ),
        ),
        (
            "Trust".into(),
            [field("trust"), field("environment"), field("realm")].join(&sep),
        ),
        (String::new(), String::new()),
        ("Cloud".into(), flag("synced", "synced", "not synced")),
        (
            "Pairing".into(),
            format!(
                "{}{sep}{}",
                flag("paired", "paired", "unpaired"),
                flag("unlocked", "unlocked", "locked")
            ),
        ),
        ("Identity".into(), theme.paint(Role::Accent, fingerprint)),
    ];
    if !value["mismatch"].is_null() {
        rows.push(("Mismatch".into(), field("mismatch")));
    }
    if value.get("enrolled").is_some() {
        rows.push(("Enrolled".into(), field("enrolled")));
    }
    human::block(theme, &rows)
}

/// Suggests the next enrollment, pairing or unlock action from the reported state.
fn status_hints(context: &Context, connection: &Connection, value: &Value) {
    if matches!(connection.identity, Identity::SelfSigned(_))
        && connection.device.kind() == DeviceKind::Emulator
    {
        context
            .output
            .event("hint", "run `ark enroll` to obtain an attested identity");
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

/// Forces cloud synchronization and reports registry state before failing an inactive check.
pub(crate) fn genuine(context: &Context) -> Result<(), Error> {
    let connection = context.connect(None)?;
    connection.client.sync(context.timing())?;
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
    context.output.document(&json!({"serial":registration.serial,"enrolled":timestamp(registration.enrolled.max(0) as u64),
        "active":registration.active(),"disabled":registration.disabled,"expired":registration.expired,"superseded":registration.superseded}))?;
    if !registration.active() {
        return Err(
            Error::new(4, "registry-inactive", "the Ark registration is inactive").hint(
                if registration.disabled {
                    "contact Dark Bio"
                } else {
                    "enroll the emulator again to obtain a fresh identity"
                },
            ),
        );
    }
    Ok(())
}

/// Installs a supplied attestation or directs online enrollment to the Ark Hub.
/// After installation, reconnects without recovery pinning to verify the new identity;
/// a reconnect failure still reports that enrollment was acknowledged.
pub(crate) fn enroll(context: &Context, args: args::Enroll) -> Result<(), Error> {
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
    let connection = if certificate.is_some() {
        context.connect_recovery(args.recovery.pubkey.as_deref())?
    } else {
        context.connect(args.recovery.pubkey.as_deref())?
    };
    let Some(certificate) = certificate else {
        if matches!(connection.identity, Identity::Attested { .. }) {
            return Err(Error::new(
                5,
                "already-enrolled",
                "the Ark already has an attested identity",
            )
            .hint("a fresh emulator identity requires a fresh emulator disk"));
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
            "online enrollment is performed at the Ark Hub",
        ));
    };
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
            print_status(context, &value)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

    #[test]
    fn status_distinguishes_unknown_state_and_keeps_mismatches_visible() {
        let theme = Theme::test(80, Color::Basic, true);
        let mut value = json!({
            "name":"Example Ark", "serial":null,
            "hardware":{"version":"Ark I","revision":"B","model":"01"},
            "firmware":{"version":"0.11.5","published":null},
            "trust":"self-signed","environment":null,"realm":null,
            "synced":null,"paired":true,"unlocked":false,
            "identity":"0123456789abcdef","pubkey":"abcdef0123456789","mismatch":"Ark II - A",
        });
        assert_eq!(
            status_block(&theme, &value),
            "  \x1b[1mExample Ark\x1b[0m  unverified\n  Hardware  Ark I, revision B, model 01\n\n  Firmware  0.11.5, published -\n  Trust     \x1b[1m! self-signed\x1b[0m \u{00b7} - \u{00b7} -\n\n  Cloud     -\n  Pairing   \x1b[1m\u{2713} paired\x1b[0m \u{00b7} \x1b[1m! locked\x1b[0m\n  Identity  \x1b[1m0123456789abcdef\x1b[0m\n  Mismatch  \x1b[1mArk II - A\x1b[0m"
        );
        let rendered = status_block(&theme, &value);
        assert!(rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains("abcdef0123456789"));
        value["synced"] = json!(false);
        assert!(status_block(&theme, &value).contains("! not synced"));
    }
}
