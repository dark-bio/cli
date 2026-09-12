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
};
use darkbio_connect::{DeviceKind, Identity, schema};
use serde_json::{Value, json};

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

pub(crate) fn status(context: &Context, recovery: args::Recovery) -> Result<(), Error> {
    let connection = context.connect_recovery(recovery.pubkey.as_deref())?;
    let value = status_value(&connection);
    context.output.document(&value)?;
    connection.require_current()?;
    status_hints(context, &connection, &value);
    Ok(())
}

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
            context.output.document(&value)
        }
        Err(error) => {
            context
                .output
                .document(&json!({"enrolled":true,"status":null}))?;
            Err(error)
        }
    }
}

pub(crate) fn timestamp(seconds: u64) -> Option<String> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub(crate) fn hub(env: darkbio_connect::trust::Environment) -> &'static str {
    use darkbio_connect::trust::Environment::*;
    match env {
        Release => "https://hub.dark.bio",
        Staging => "https://hub.darkbio.xyz",
        Develop => "https://hub.darkbio.dev",
    }
}
