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
    connection
        .client
        .pair(context.timing(), |stage| match stage {
            PairingProgress::Rendezvous {
                colo,
                secret,
                deadline: _,
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
                context
                    .output
                    .event("approve", format!("scan with Ark Companion: {url}"));
                if context.output.human()
                    && let Ok(qr) = qrcode::QrCode::new(url.as_bytes())
                {
                    context.output.event(
                        "approve",
                        format!(
                            "\n{}",
                            qr.render::<qrcode::render::unicode::Dense1x2>()
                                .quiet_zone(true)
                                .build()
                        ),
                    );
                }
            }
            PairingProgress::Identity => context.output.event("progress", "exchanging identities"),
            PairingProgress::Storage => context.output.event("progress", "exchanging storage keys"),
            PairingProgress::Approval => context
                .output
                .event("approve", "confirm the colours on the Ark and your phone"),
            PairingProgress::Formatting => context
                .output
                .event("progress", "preparing encrypted storage"),
        })?;
    let serial = match &connection.identity {
        darkbio_connect::Identity::Attested { device, .. } => Some(&device.serial),
        _ => None,
    };
    context
        .output
        .document(&json!({"serial": serial, "paired": true}))
}
