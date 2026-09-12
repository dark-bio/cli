// ark: command line for Dark Bio Arks
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Stable caller actions around opaque application refusals.

use darkbio_connect::{Error as ConnectError, schema, wire};
use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct Error {
    pub class: u8,
    pub code: &'static str,
    pub message: String,
    pub hints: Vec<String>,
    pub remote: Option<schema::Error>,
}

impl Error {
    pub fn new(class: u8, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            code,
            message: message.into(),
            hints: Vec::new(),
            remote: None,
        }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }
    pub fn json(&self) -> Value {
        let mut value = json!({"code": self.code, "message": self.message});
        if let Some(remote) = &self.remote {
            value["remote"] = json!({"code": remote.code, "message": remote.msg});
        }
        value
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

impl From<ConnectError> for Error {
    fn from(error: ConnectError) -> Self {
        use ConnectError::*;
        match error {
            Remote(remote) => {
                let code = match schema::ReservedErrors::try_from(remote.code as i32)
                    .ok()
                    .filter(|kind| *kind as u64 == remote.code)
                {
                    Some(schema::ReservedErrors::Unauthorized) => "approval-denied",
                    Some(schema::ReservedErrors::Unconfirmed) => "approval-timeout",
                    Some(schema::ReservedErrors::Unsupported) => "unsupported",
                    Some(schema::ReservedErrors::Unknown) => "unknown",
                    Some(schema::ReservedErrors::Unavailable) => "unavailable",
                    Some(schema::ReservedErrors::Unanswered) => "unanswered",
                    _ => "ark",
                };
                let class = if matches!(code, "approval-denied" | "approval-timeout") {
                    6
                } else {
                    5
                };
                let mut error = Self::new(class, code, remote.msg.clone());
                if code == "unknown" {
                    error
                        .hints
                        .push("update the Ark firmware and this tool".into());
                }
                error.remote = Some(remote);
                error
            }
            Timeout => Self::new(7, "timeout", "a wait for a response expired"),
            PairingExpired => Self::new(6, "approval-timeout", "pairing timed out")
                .hint("run `ark pair` to open a new pairing window"),
            MissingEnvironment => Self::new(4, "environment-unknown", "cloud environment unknown")
                .hint("select an environment with --env"),
            NotFound | NoMatch(_) => {
                Self::new(3, "no-device", error.to_string()).hint("run `ark devices`")
            }
            Ambiguous(locators) => {
                Self::new(3, "ambiguous-device", "several Arks match").hint(format!(
                    "select one with --device: {}",
                    locators
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Busy(_) => Self::new(3, "device-busy", "the Ark is in use")
                .hint("close the Ark Hub browser tab or the other ark command"),
            Usb(cause) => {
                let cause = std::io::Error::from(cause);
                let mut error = Self::new(3, "device-unreachable", cause.to_string());
                if cfg!(target_os = "linux") && cause.kind() == std::io::ErrorKind::PermissionDenied
                {
                    error.hints.push(usb_hint());
                }
                error
            }
            Closed | Disconnected(_) => Self::new(3, "disconnected", error.to_string()),
            Untrusted(env) => Self::new(
                3,
                "handshake-failed",
                format!("attestation signer for {env} is not trusted"),
            ),
            Handshake(wire::protocol::Error::Transport(cause)) => {
                if let wire::transport::Error::HandshakeFailed(reason) = cause.as_ref() {
                    Self::new(3, "handshake-failed", reason.clone())
                } else {
                    Self::new(3, "handshake-failed", cause.to_string())
                }
            }
            Handshake(_) => Self::new(3, "handshake-failed", error.to_string()),
            Cloud(message) => Self::new(4, "cloud-unreachable", message),
            Relay(message) => Self::new(4, "cloud-unreachable", message),
            Pairing(message) => Self::new(4, "pairing-failed", message),
            ProofRejected => Self::new(4, "proof-rejected", "the cloud rejected the device proof")
                .hint("run `ark doctor`"),
            Integrity(message) => Self::new(1, "file-rejected", message),
            FirmwareRead(error) => {
                let kind = error.kind();
                let message = error.to_string();
                match error
                    .into_inner()
                    .and_then(|inner| inner.downcast::<Self>().ok())
                {
                    Some(error) => *error,
                    None if kind == std::io::ErrorKind::TimedOut => {
                        Self::new(7, "timeout", message)
                    }
                    None => Self::new(4, "cloud-unreachable", message),
                }
            }
            DatasetRead(error) | ExecutionRead(error) => {
                Self::new(1, "file-unreadable", error.to_string())
            }
            Dataset(message) => Self::new(5, "ark", message),
            Firmware(message) => Self::new(5, "ark", message),
            Execution(message) => Self::new(5, "ark", message),
            _ => Self::new(3, "device-unreachable", error.to_string()),
        }
    }
}

pub(crate) fn usb_hint() -> String {
    format!(
        "create /etc/udev/rules.d/70-darkbio-ark.rules containing: {}; then run `sudo udevadm control --reload-rules` and reconnect the Ark",
        include_str!("../packaging/70-darkbio-ark.rules").trim()
    )
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(1, "io", error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_trust_and_pairing_errors_retain_caller_actions() {
        let err = Error::from(ConnectError::Untrusted(
            darkbio_connect::trust::Environment::Develop,
        ));
        assert_eq!((err.class, err.code), (3, "handshake-failed"));
        assert_eq!(err.message, "attestation signer for develop is not trusted");
        assert!(err.hints.is_empty());
        let err = Error::from(ConnectError::Handshake(
            wire::transport::Error::HandshakeFailed("attestation signed by an unknown key".into())
                .into(),
        ));
        assert_eq!((err.class, err.code), (3, "handshake-failed"));
        assert_eq!(err.message, "attestation signed by an unknown key");
        assert!(err.hints.is_empty());
        let err = Error::from(ConnectError::PairingExpired);
        assert_eq!((err.class, err.code), (6, "approval-timeout"));
        let err = Error::from(ConnectError::Pairing("companion disconnected".into()));
        assert_eq!(err.code, "pairing-failed");
        assert_eq!(err.message, "companion disconnected");
    }

    /// An application code can share low bits with a reserved code without
    /// being interpreted by the host. Its message and full number survive.
    #[test]
    fn application_errors_are_opaque() {
        for code in [
            0x506,
            u64::MAX,
            (1u64 << 32) | schema::ReservedErrors::Unsupported as u64,
        ] {
            let error: Error =
                ConnectError::Remote(schema::Error::new(code, "owner's verdict")).into();
            assert_eq!(error.class, 5);
            assert_eq!(error.code, "ark");
            assert!(error.hints.is_empty());
            assert_eq!(error.json()["remote"]["code"], code);
            assert_eq!(error.json()["remote"]["message"], "owner's verdict");
        }
    }

    #[test]
    fn download_error_retains_required_caller_action() {
        let source = Error::new(4, "login-required", "sign in to the package host")
            .hint("run cloudflared access login");
        let error: Error = ConnectError::FirmwareRead(std::io::Error::other(source)).into();
        assert_eq!(error.code, "login-required");
        assert_eq!(error.hints.len(), 1);
        let error: Error = ConnectError::FirmwareRead(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "stalled",
        ))
        .into();
        assert_eq!(error.class, 7);
        assert_eq!(error.message, "stalled");
    }
    #[test]
    fn reserved_approval_outcomes_are_distinct() {
        for (code, name) in [
            (schema::ReservedErrors::Unauthorized, "approval-denied"),
            (schema::ReservedErrors::Unconfirmed, "approval-timeout"),
        ] {
            let error = Error::from(ConnectError::Remote(schema::Error::reserved(
                code,
                "owner's verdict",
            )));
            assert_eq!(error.class, 6);
            assert_eq!(error.code, name);
            assert_eq!(error.message, "owner's verdict");
        }
    }
}
