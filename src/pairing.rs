// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Pairing links and terminal presentation of the owner's steps.

use crate::{context::Context, error::Error, output::Output};
use darkbio_connect::{
    DeviceKind, PairingProgress,
    trust::{Environment, Realm},
};
use serde_json::json;

/// Pairs an unpaired Ark, translating connection library stages into the
/// owner's scan and approval instructions.
///
/// Link construction and terminal presentation stay in the CLI.
pub(crate) fn run(context: &Context) -> Result<(), Error> {
    // Pairing needs an unpaired Ark and a known cloud environment
    let connection = context.connect(None)?;
    if connection.info.paired {
        return Err(Error::new(5, "already-paired", "the Ark is already paired")
            .hint("the reset button removes pairing and erases the data"));
    }
    let env = connection.env.ok_or_else(|| {
        Error::new(4, "environment-unknown", "cloud environment unknown")
            .hint("select one with --env")
    })?;

    // The link names the environment's app, the signer's fingerprint and the
    // realm, falling back to the device kind without an attested one
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

    // Every output follows the same sequence of scan, exchange and approval
    let result = connection
        .client
        .pair(context.timing(), |event| match event {
            PairingProgress::Rendezvous {
                colo,
                secret,
                deadline: _,
                fingerprint,
            } => {
                // Colo is routing supplied by the cloud, never a URL or a host
                // name, so anything but letters and digits is percent-encoded
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
                context.output.pairing(&url);
            }
            PairingProgress::Identity => context.output.stage("identity", "exchanging identities"),
            PairingProgress::Storage => context.output.stage("storage", "exchanging storage keys"),
            PairingProgress::Approval => {
                approve(&context.output, connection.device.kind());
            }
            PairingProgress::Formatting => context
                .output
                .stage("formatting", "preparing encrypted storage"),
        });

    // Complete the last stage only when pairing succeeded
    if result.is_ok() {
        context.output.end_stage(None, None);
    }
    result?;

    // Report the serial of an attested Ark, or null for any other
    let serial = match &connection.identity {
        darkbio_connect::Identity::Attested { device, .. } => Some(&device.serial),
        _ => None,
    };
    context
        .output
        .document(&json!({"serial": serial, "paired": true}))
}

/// Describes the color check and button location for the connected device kind.
fn approve(output: &Output, kind: DeviceKind) {
    output.approve("pair", "button", match kind {
        DeviceKind::Hardware => "if the colors on the Ark and your phone match, press the Ark's button with the pin that came with it, in the pinhole under the bottom right LED",
        DeviceKind::Emulator => "if the colors on the emulated Ark and your phone match, press its button in the Ark Emulator window, or run `ark-emulator button press --release-after 0`",
    });
}
