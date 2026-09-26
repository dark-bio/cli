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
use darkbio_clock::Clock;
use darkbio_connect::{
    Ark, Client, Device, Identity, Timing, TrustMode, schema, trust::Environment,
};
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

/// Invocation policy and shared output, kept outside the reusable connector.
pub(crate) struct Context {
    /// Parsed global flags controlling selection, prompts and wait allowances.
    pub options: Options,
    /// Result and event streams shared with progress and signal handlers.
    pub output: Output,
    /// Tracks the active session and cancellable work for process interruption.
    pub interrupt: crate::interrupt::Interrupt,
}

/// Owned session and snapshots used by one command's policy checks.
/// Device info is not refreshed automatically after mutations.
pub(crate) struct Connection {
    /// Keeps the wire session and its lazy cloud services alive.
    pub ark: Ark,
    /// Typed request handle bound to the owned session.
    pub client: Client,
    /// Trust outcome established during the handshake.
    pub identity: Identity,
    /// Device state captured when the session opened.
    pub info: schema::DeviceInfoResponse,
    /// Discovery record retained for labels and reconnecting after reboot.
    pub device: Device,
    /// Selected cloud route, independent of the attestation's trust outcome.
    pub env: Option<Environment>,
}

impl Connection {
    /// Enforces the CLI protocol minimum with a hardware or emulator upgrade hint.
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
    /// Gives each machine wait the CLI allowance without bounding the full command.
    pub fn timing(&self) -> Timing {
        Timing::inactivity(Duration::from_secs(self.options.timeout))
    }
    /// Starts one fixed wait budget on a connection's clock, for operations
    /// that need a single bound.
    pub fn deadline(&self, clock: &Clock) -> Instant {
        clock.now() + Duration::from_secs(self.options.timeout)
    }
    /// Permits stdin prompts only for a terminal outside JSON and no-input modes.
    pub fn interactive(&self) -> bool {
        !self.options.no_input && !self.output.json() && io::stdin().is_terminal()
    }

    /// Enumerates both device kinds and reports partial source failures as warnings.
    pub fn discover(&self) -> darkbio_connect::Discovery {
        let found = darkbio_connect::list();
        for error in &found.errors {
            self.output.event("warning", error.to_string());
        }
        self.output
            .event("step", format!("discovered {} Arks", found.devices.len()));
        found
    }

    /// Selects and opens an Ark, then enforces the CLI's firmware compatibility gate.
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

    /// Opens an already selected endpoint and reads its state without the version gate.
    pub fn open(&self, device: Device, pubkey: Option<&str>) -> Result<Connection, Error> {
        self.open_until(device, pubkey, None)
    }

    /// Opens an endpoint with CLI routing precedence and records it for interruption.
    /// An optional reboot deadline bounds device-info I/O; transport establishment
    /// and the wire handshake retain their own connection timeouts. The output
    /// and the login helper time their waits on the new connection's clock.
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
        let (mut ark, identity) = device.connect_with_env(&trust, |identity| {
            environment(self.options.env, identity, device.env())
        })?;
        let client = ark.client();
        self.output.connection(client.clock());
        let env = environment(self.options.env, &identity, device.env());
        if let Identity::Attested { env: attested, .. } = &identity
            && self.options.env.is_some_and(|env| env != *attested)
        {
            self.output.event(
                "warning",
                format!("--env overrides the attested {attested} environment"),
            );
        }
        self.output.environment(env);
        if env != Environment::Release {
            ark.set_cloud_auth(crate::access::Login::new(self, client.clock()));
        }
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

    /// Requires pairing and unlock, optionally prompting or honoring --unlock.
    /// A dry run never unlocks implicitly. Successful unlock leaves the original
    /// device-info snapshot unchanged; callers can continue the requested operation.
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
                    if self.output.terminal() {
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

    /// Attaches the relay before announcing approval and requesting unlock.
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

    /// Prompts for a yes/no answer; EOF and unrecognized input decline.
    /// The caller must first check whether this invocation permits input.
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
    identity: &Identity,
    reported: Option<&str>,
) -> Environment {
    let attested = match identity {
        Identity::Attested { env, .. } => Some(*env),
        _ => None,
    };
    overridden
        .or(attested)
        .or_else(|| reported.and_then(|env| parse_env(env).ok()))
        .unwrap_or(Environment::Release)
}

/// Parses an explicit recovery key, otherwise accepting enabled roots or self-signing.
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

/// Opens a nonempty regular file and returns its current size without reading it.
/// Upload workflows separately detect files that change after this snapshot.
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
            assert_eq!(environment(None, &identity, None), env);
            assert_eq!(environment(None, &identity, Some("develop")), env);
            assert_eq!(environment(None, &identity, Some("release")), env);
            assert_eq!(
                environment(Some(Environment::Staging), &identity, Some("develop")),
                Environment::Staging
            );
        }
        for identity in [Identity::SelfSigned(key.clone()), Identity::Recovered(key)] {
            assert_eq!(
                environment(None, &identity, Some("develop")),
                Environment::Develop
            );
            assert_eq!(
                environment(None, &identity, Some("staging")),
                Environment::Staging
            );
            assert_eq!(environment(None, &identity, None), Environment::Release);
            assert_eq!(
                environment(None, &identity, Some("invalid")),
                Environment::Release
            );
            assert_eq!(
                environment(Some(Environment::Release), &identity, Some("develop")),
                Environment::Release
            );
        }
    }
}
