// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Pairing links and terminal presentation of the owner's steps.

use crate::{context::Context, error::Error};
use darkbio_connect::{
    PairingProgress,
    trust::{Environment, Realm},
};
use serde_json::json;
use std::time::{Duration, UNIX_EPOCH};

/// Pairs an unpaired Ark, translating connector stages into the owner's scan and
/// approval instructions. Link construction and terminal presentation stay in the CLI.
pub(crate) fn run(context: &Context) -> Result<(), Error> {
    let connection = context.connect(None)?;
    if connection.info.paired {
        return Err(Error::new(5, "already-paired", "the Ark is already paired")
            .hint("the reset button removes pairing and erases the data"));
    }
    let env = connection.env.ok_or_else(|| {
        Error::new(4, "environment-unknown", "cloud environment unknown")
            .hint("select one with --env")
    })?;
    let origin = match env {
        Environment::Release => "https://app.dark.bio",
        Environment::Staging => "https://app.darkbio.xyz",
        Environment::Develop => "https://app.darkbio.dev",
    };
    let signer = hex::encode(connection.identity.key().fingerprint().to_bytes());
    let realm = connection
        .identity
        .realm()
        .unwrap_or(match connection.device.kind() {
            darkbio_connect::DeviceKind::Hardware => Realm::Hardware,
            darkbio_connect::DeviceKind::Emulator => Realm::Emulator,
        });
    let mut previous = None;
    let result = connection
        .client
        .pair(context.timing(), |event| match event {
            PairingProgress::Rendezvous {
                colo,
                secret,
                deadline,
                fingerprint,
            } => {
                // Colo is routing supplied by the cloud, never a URL or a host name.
                let colo: String = colo
                    .bytes()
                    .map(|byte| {
                        if byte.is_ascii_alphanumeric() {
                            (byte as char).to_string()
                        } else {
                            format!("%{byte:02X}")
                        }
                    })
                    .collect();
                let url = format!(
                    "{origin}/pair/{signer}?crypto={}&colo={colo}&secret={}{}",
                    hex::encode(fingerprint),
                    hex::encode(secret),
                    if realm == Realm::Emulator {
                        "&realm=sandbox"
                    } else {
                        ""
                    }
                );
                if context.output.terminal() {
                    context
                        .output
                        .pairing(&url, UNIX_EPOCH + Duration::from_secs(deadline));
                    previous = Some("rendezvous");
                } else {
                    context
                        .output
                        .event("approve", format!("scan with Ark Companion: {url}"));
                }
            }
            PairingProgress::Identity => {
                stage(context, &mut previous, "identity", "exchanging identities")
            }
            PairingProgress::Storage => {
                stage(context, &mut previous, "storage", "exchanging storage keys")
            }
            PairingProgress::Approval => {
                if let Some(name) = previous.take() {
                    context.output.stage(name, true);
                }
                context
                    .output
                    .event("approve", "confirm the colours on the Ark and your phone");
                if context.output.terminal() {
                    previous = Some("approval");
                }
            }
            PairingProgress::Formatting => stage(
                context,
                &mut previous,
                "formatting",
                "preparing encrypted storage",
            ),
        });
    if result.is_ok()
        && let Some(name) = previous
    {
        context.output.stage(name, true);
    }
    result?;
    let serial = match &connection.identity {
        darkbio_connect::Identity::Attested { device, .. } => Some(&device.serial),
        _ => None,
    };
    context
        .output
        .document(&json!({"serial": serial, "paired": true}))
}

/// Completes the previous human stage or emits the corresponding plain progress event.
fn stage(
    context: &Context,
    previous: &mut Option<&'static str>,
    name: &'static str,
    message: &str,
) {
    if context.output.terminal() {
        if let Some(name) = previous.take() {
            context.output.stage(name, true);
        }
        context.output.stage(name, false);
        *previous = Some(name);
    } else {
        context.output.event("progress", message);
    }
}
