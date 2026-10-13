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

    // Narrate the scan, the exchanges and the button approval, the same in
    // every mode
    let result = connection.client.pair(context.timing(), |event| {
        narrate(
            &context.output,
            connection.device.kind(),
            origin,
            &signer,
            realm,
            event,
        )
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

/// Narrates one step of pairing, from the scan through preparing the storage.
fn narrate(
    output: &Output,
    kind: DeviceKind,
    origin: &str,
    signer: &str,
    realm: Realm,
    event: PairingProgress,
) {
    match event {
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
            output.pairing(&url);
        }
        PairingProgress::Identity => output.stage(
            "identity",
            "Exchanging identities",
            Some("Exchanged identities"),
        ),
        PairingProgress::Storage => output.stage(
            "storage",
            "Exchanging storage keys",
            Some("Exchanged storage keys"),
        ),
        PairingProgress::Approval => {
            approve(output, kind);
        }
        PairingProgress::Formatting => output.stage(
            "formatting",
            "Preparing encrypted storage",
            Some("Prepared encrypted storage"),
        ),
    }
}

/// Describes the color check and button location for the connected device kind.
fn approve(output: &Output, kind: DeviceKind) {
    output.approve("pair", Some("button"), match kind {
        DeviceKind::Hardware => "Approve the pairing on the Ark if its colors match your phone. Press its button with the pin that came with it, in the pinhole under the bottom right LED",
        DeviceKind::Emulator => "Approve the pairing in the Ark Emulator window if its colors match your phone. Press its button there, or run `ark-emulator button press`",
    });
}

/// Tests of the pairing narration.
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Pairing prints the code above the scan's count-up timer, and narrates
    /// each exchange.
    #[test]
    fn test_pairing_narration() {
        let captures =
            crate::testing::narration("pairing::tests::test_pairing_narration", |output, clock| {
                let show = |stage| {
                    narrate(
                        &output,
                        DeviceKind::Hardware,
                        "https://app.dark.bio",
                        "sample",
                        Realm::Hardware,
                        stage,
                    )
                };
                show(PairingProgress::Rendezvous {
                    colo: "test".into(),
                    secret: [0; 32],
                    fingerprint: vec![1; 4],
                    deadline: clock.clock().now() + Duration::from_secs(600),
                });
                output.finish();
                clock.advance(Duration::from_secs(12));
                output.redraw();
                clock.advance(Duration::from_secs(2));
                show(PairingProgress::Identity);
                output.finish();
                clock.advance(Duration::from_secs(1));
                show(PairingProgress::Storage);
                output.finish();
                clock.advance(Duration::from_secs(2));
                show(PairingProgress::Approval);
                output.finish();
                clock.advance(Duration::from_secs(9));
                show(PairingProgress::Formatting);
                output.finish();
                clock.advance(Duration::from_secs(30));
                output.end_stage(None, None);
                approve(&output, DeviceKind::Emulator);
                output.end_stage(None, None);
            });
        for captured in &captures {
            // JSON gives the link in the instruction, where reading output shows
            // the code above it
            let scan = if captured.json {
                "Approve the pairing by opening https://app.dark.bio/pair/sample?"
            } else {
                "Approve the pairing by scanning this code in Ark Companion"
            };
            captured.contains(&[
                scan, "Scanned on your phone · 14s",
                "Exchanging identities", "Exchanged identities · 1s", "Exchanging storage keys", "Exchanged storage keys · 2s",
                "Approve the pairing on the Ark if its colors match your phone. Press its button with the pin that came with it, in the pinhole under the bottom right LED",
                "Approved with the Ark's button · 9s", "Preparing encrypted storage", "Prepared encrypted storage · 30s",
                "Approve the pairing in the Ark Emulator window if its colors match your phone. Press its button there, or run `ark-emulator button press`",
            ]);
            captured.approvals(&[
                ("pair", Some("phone")),
                ("pair", Some("button")),
                ("pair", Some("button")),
            ]);
            assert!(
                !captured.stderr.contains("left"),
                "pairing has no countdown"
            );
            if captured.terminal {
                assert!(
                    captured.stderr.contains("Ark Companion · 12s"),
                    "the pairing timer counts up"
                );
            }
        }
        if !captures.is_empty() {
            // A terminal and a pipe print the same QR code and link
            let terminal = captures[0].stderr.split_once("! Approve").unwrap().0;
            let pipe = captures[1].stderr.split_once("! Approve").unwrap().0;
            assert_eq!(terminal, pipe, "the pairing code matches in both views");
            assert!(pipe.contains('█'), "the pipe keeps the QR code");
            assert!(
                pipe.contains("\n  https://app.dark.bio/pair/sample?"),
                "the link follows the code"
            );
            assert!(
                pipe.ends_with("\n\n"),
                "the instruction follows a blank line"
            );
        }
    }
}
