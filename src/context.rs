// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! CLI selection, environment policy and state prerequisites.

use crate::{
    args::{Options, parse_env},
    error::Error,
    output::Output,
};
use darkbio_connect::{
    Ark, Client, Device, Identity, Timing, TrustMode, schema, trust::Environment,
};
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) struct Context {
    pub options: Options,
    pub output: Output,
    pub interrupt: crate::interrupt::Interrupt,
}

pub(crate) struct Connection {
    pub ark: Ark,
    pub client: Client,
    pub identity: Identity,
    pub info: schema::DeviceInfoResponse,
    pub device: Device,
    pub env: Option<Environment>,
}

impl Connection {
    pub fn require_current(&self) -> Result<(), Error> {
        crate::firmware::check_compatibility(&self.info).map_err(|err| {
            err.hint(match self.device.kind() {
                darkbio_connect::DeviceKind::Hardware => "run `ark firmware update`",
                darkbio_connect::DeviceKind::Emulator => "update the emulator app",
            })
        })
    }
}

impl Context {
    pub fn timing(&self) -> Timing {
        Timing::inactivity(Duration::from_secs(self.options.timeout))
    }
    pub fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs(self.options.timeout)
    }
    pub fn interactive(&self) -> bool {
        !self.options.no_input && !self.output.json() && io::stdin().is_terminal()
    }

    pub fn discover(&self) -> darkbio_connect::Discovery {
        let found = darkbio_connect::list();
        for error in &found.errors {
            self.output.event("warning", error.to_string());
        }
        self.output
            .event("step", format!("discovered {} Arks", found.devices.len()));
        found
    }

    pub fn connect(&self, pubkey: Option<&str>) -> Result<Connection, Error> {
        let connection = self.connect_recovery(pubkey)?;
        connection.require_current()?;
        Ok(connection)
    }

    /// Only diagnostics and the commands that upgrade or enroll an old device
    /// may cross the compatibility gate.
    pub fn connect_recovery(&self, pubkey: Option<&str>) -> Result<Connection, Error> {
        let found = self.discover();
        let device = found.select(self.options.device.as_deref())?.clone();
        self.open(device, pubkey)
    }

    pub fn open(&self, device: Device, pubkey: Option<&str>) -> Result<Connection, Error> {
        self.open_until(device, pubkey, None)
    }

    /// Reboot verification also bounds device-info requests by its fixed window.
    pub fn open_until(
        &self,
        device: Device,
        pubkey: Option<&str>,
        deadline: Option<Instant>,
    ) -> Result<Connection, Error> {
        let timing = deadline.map_or(self.timing(), |deadline| {
            self.timing().with_deadline(deadline)
        });
        let trust = trust(pubkey)?;
        let selected = environment(self.options.env, None, device.env());
        let (mut ark, mut identity) = device.connect_with_env(&trust, selected)?;
        let env = environment(self.options.env, Some(&identity), device.env());
        if let Identity::Attested { env: attested, .. } = &identity {
            let attested = *attested;
            if self.options.env.is_some_and(|env| env != attested) {
                self.output.event(
                    "warning",
                    format!("--env overrides the attested {attested} environment"),
                );
            } else if self.options.env.is_none() {
                // Routing is fixed when connect opens a session. A verified
                // attestation supersedes any provisional launcher default.
                if selected != attested {
                    let key = identity.key().clone();
                    drop(ark);
                    (ark, identity) = device.connect_with_env(&trust, attested)?;
                    if identity.key().to_bytes() != key.to_bytes() {
                        return Err(Error::new(
                            3,
                            "handshake-failed",
                            "the Ark's identity changed while connecting",
                        ));
                    }
                }
            }
        }
        self.output.environment(env);
        let client = ark.client();
        self.interrupt.connection(client.clone(), ark.closer());
        let info = client.call(schema::DeviceInfoRequest {}, timing)?;
        self.output.event("step", "connected and authenticated");
        Ok(Connection {
            ark,
            client,
            identity,
            info,
            device,
            env: Some(env),
        })
    }

    pub fn require_unlocked(&self, connection: &Connection, dry_run: bool) -> Result<(), Error> {
        let state = &connection.info;
        if !state.paired {
            return Err(Error::new(5, "not-paired", "the Ark is not paired").hint("run `ark pair`"));
        }
        if state.unlocked {
            return Ok(());
        }
        if dry_run {
            return Err(Error::new(5, "locked", "the Ark is locked")
                .hint("run `ark unlock` separately before the dry run"));
        }
        if self.options.unlock
            || (self.interactive()
                && self.confirm(
                    if self.output.human() {
                        "Unlock the Ark first?"
                    } else {
                        "The Ark is locked. Unlock it now? You will approve on your phone."
                    },
                    true,
                )?)
        {
            return self.unlock(connection);
        }
        Err(Error::new(5, "locked", "the Ark is locked")
            .hint("run `ark unlock`, or add --unlock to unlock first"))
    }

    pub fn unlock(&self, connection: &Connection) -> Result<(), Error> {
        connection.client.attach_relay(self.timing())?;
        self.output
            .event("approve", "unlock (Ark Companion on your phone)");
        connection
            .client
            .call(schema::UnlockRequest {}, self.timing())?;
        self.output.finish();
        self.output.human_event("progress", "unlocked");
        Ok(())
    }

    pub fn confirm(&self, message: &str, default: bool) -> Result<bool, Error> {
        self.output.prompt(message, default)?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Ok(false);
        }
        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }
}

/// Routing does not change the identity established by the handshake.
fn environment(
    overridden: Option<Environment>,
    identity: Option<&Identity>,
    reported: Option<&str>,
) -> Environment {
    let attested = match identity {
        Some(Identity::Attested { env, .. }) => Some(*env),
        _ => None,
    };
    overridden
        .or(attested)
        .or_else(|| reported.and_then(|env| parse_env(env).ok()))
        .unwrap_or(Environment::Release)
}

fn trust(pubkey: Option<&str>) -> Result<TrustMode, Error> {
    let Some(encoded) = pubkey else {
        return Ok(TrustMode::RootOrSelf);
    };
    let bytes =
        hex::decode(encoded).map_err(|_| Error::new(1, "invalid-key", "invalid --pubkey hex"))?;
    let bytes = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::new(1, "invalid-key", "invalid --pubkey length"))?;
    let key = darkbio_connect::wire::crypto::xdsa::PublicKey::from_bytes(bytes)
        .map_err(|_| Error::new(1, "invalid-key", "invalid --pubkey"))?;
    Ok(TrustMode::Recover(Box::new(key)))
}

pub(crate) fn open_file(path: &Path) -> Result<(std::fs::File, u64), Error> {
    let file = std::fs::File::open(path).map_err(|err| {
        Error::new(
            1,
            if err.kind() == io::ErrorKind::NotFound {
                "file-not-found"
            } else {
                "file-unreadable"
            },
            format!("{}: {err}", path.display()),
        )
    })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::new(
            1,
            "file-unreadable",
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() == 0 {
        return Err(Error::new(
            1,
            "file-empty",
            format!("{} is empty", path.display()),
        ));
    }
    Ok((file, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkbio_connect::{
        trust,
        wire::crypto::{cwt::claims::eat, xdsa},
    };

    #[test]
    fn environment_follows_override_attestation_launcher_release() {
        let key = xdsa::SecretKey::generate().public_key();
        for env in [
            Environment::Release,
            Environment::Staging,
            Environment::Develop,
        ] {
            let identity = Identity::Attested {
                env,
                device: trust::device::Device {
                    realm: trust::Realm::Hardware,
                    identity: key.clone(),
                    serial: "test-serial".into(),
                    oem: eat::Oemid::new_pen(65145),
                    model: b"Ark".to_vec(),
                    version: "1.0".into(),
                    issued: 0,
                    expiry: None,
                },
            };
            assert_eq!(environment(None, Some(&identity), Some("develop")), env);
            assert_eq!(environment(None, Some(&identity), Some("release")), env);
            assert_eq!(
                environment(Some(Environment::Staging), Some(&identity), Some("develop")),
                Environment::Staging
            );
        }
        for identity in [Identity::SelfSigned(key.clone()), Identity::Recovered(key)] {
            assert_eq!(
                environment(None, Some(&identity), Some("develop")),
                Environment::Develop
            );
            assert_eq!(
                environment(None, Some(&identity), Some("staging")),
                Environment::Staging
            );
            assert_eq!(
                environment(None, Some(&identity), None),
                Environment::Release
            );
            assert_eq!(
                environment(None, Some(&identity), Some("invalid")),
                Environment::Release
            );
            assert_eq!(
                environment(Some(Environment::Release), Some(&identity), Some("develop")),
                Environment::Release
            );
        }
    }
}
