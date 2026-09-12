// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Cloudflare Access credentials for internal package hosts.

use crate::{context::Context, error::Error};
use darkbio_connect::Error as ConnectError;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub(super) fn cached(context: &Context, origin: &str) -> Option<String> {
    token(
        Command::new("cloudflared").args(["access", "token", "--app", origin]),
        context.deadline(),
    )
    .ok()
}

pub(super) fn authenticate(context: &Context, origin: &str) -> Result<String, Error> {
    let required = || {
        Error::new(4, "login-required", "package access requires login")
            .hint(format!("run `cloudflared access login --app {origin}`"))
    };
    if !context.interactive() {
        return Err(required());
    }
    context.output.event(
        "note",
        format!("opening the browser to sign in to {origin}"),
    );
    token(
        Command::new("cloudflared").args(["access", "login", "--app", origin]),
        Instant::now() + Duration::from_secs(600),
    )
    .map_err(|err| {
        Error::new(4, "login-required", err.to_string())
            .hint(format!("run `cloudflared access login --app {origin}`"))
    })
}

pub(super) fn challenge(origin: &str, redirect: &str) -> bool {
    let Some(host) = origin.strip_prefix("https://") else {
        return false;
    };
    redirect.split(['?', '#']).next()
        == Some(
            format!("https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/{host}").as_str(),
        )
}

/// Captures credentials without forwarding helper output to the terminal.
/// Expiration kills and reaps the helper, including while its output is blocked.
fn token(command: &mut Command, deadline: Instant) -> Result<String, ConnectError> {
    remaining(deadline)?;
    let mut child = Child(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ConnectError::Cloud(
                        "Cloudflare Access requires cloudflared; install it and retry the command"
                            .into(),
                    )
                } else {
                    ConnectError::Cloud(format!("could not start cloudflared: {error}"))
                }
            })?,
    );
    let stdout = child.0.stdout.take().expect("piped helper stdout");
    let (send, recv) = mpsc::channel();
    thread::Builder::new()
        .name("access-token".into())
        .spawn(move || {
            let mut output = Vec::new();
            let result = stdout
                .take(64 * 1024 + 1)
                .read_to_end(&mut output)
                .map(|_| output);
            let _ = send.send(result);
        })
        .map_err(ConnectError::Worker)?;
    let status = loop {
        let remaining = remaining(deadline)?;
        if let Some(status) = child.0.try_wait().map_err(|error| {
            ConnectError::Cloud(format!("could not wait for cloudflared: {error}"))
        })? {
            break status;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    if !status.success() {
        return Err(ConnectError::Cloud(format!(
            "cloudflared login failed ({status})"
        )));
    }
    let output = recv
        .recv_timeout(remaining(deadline)?)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => ConnectError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => {
                ConnectError::Cloud("cloudflared output reader stopped".into())
            }
        })?
        .map_err(|error| {
            ConnectError::Cloud(format!("could not read cloudflared token: {error}"))
        })?;
    let token = String::from_utf8_lossy(&output);
    let token = token.trim();
    if output.len() > 64 * 1024
        || token.split('.').count() != 3
        || !token.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
    {
        return Err(ConnectError::Cloud(
            "cloudflared returned an invalid application token".into(),
        ));
    }
    Ok(token.to_owned())
}

fn remaining(deadline: Instant) -> Result<Duration, ConnectError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(ConnectError::Timeout)
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_scoped_to_our_tenant_and_package_host() {
        assert!(challenge(
            "https://pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.dev?redirect_url=%2Fimgs%2Farkos.pkgs"
        ));
        for redirect in [
            "https://foreign.invalid/cdn-cgi/access/login/pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com.evil.invalid/cdn-cgi/access/login/pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.xyz",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.dev/extra",
        ] {
            assert!(!challenge("https://pkg.darkbio.dev", redirect));
        }
    }
    /// Helper failures and malformed stdout are reported without including the
    /// token. The tests use a local child instead of opening a browser.
    #[cfg(unix)]
    #[test]
    fn test_token() {
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            token(
                Command::new("sh").args(["-c", "printf 'e30.e30.signature\\n'"]),
                deadline
            )
            .unwrap(),
            "e30.e30.signature"
        );
        for script in [
            "exit 0",
            "printf 'private-token'",
            "printf 'e30.e30.private-token'; exit 1",
            "head -c 70000 /dev/zero",
        ] {
            let error = token(Command::new("sh").args(["-c", script]), deadline).unwrap_err();
            assert!(!error.to_string().contains("private-token"));
        }
        let error = token(
            &mut Command::new("/nonexistent/ark-test-cloudflared"),
            deadline,
        )
        .unwrap_err();
        assert!(error.to_string().contains("install it"));
    }

    /// An expired deadline never starts a helper; a running helper is killed
    /// promptly instead of surviving until its own login timeout.
    #[cfg(unix)]
    #[test]
    fn test_deadline() {
        assert!(matches!(
            token(
                &mut Command::new("/nonexistent/ark-test-cloudflared"),
                Instant::now()
            ),
            Err(ConnectError::Timeout)
        ));
        let start = Instant::now();
        assert!(matches!(
            token(
                Command::new("sh").args(["-c", "exec sleep 5"]),
                start + Duration::from_millis(50)
            ),
            Err(ConnectError::Timeout)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
