// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Cloudflare Access login for internal API and package hosts.

use crate::{context::Context, error::Error};
use darkbio_connect::Error as ConnectError;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use ureq::http::{HeaderMap, HeaderValue, StatusCode};

/// Prompt policy copied into a connection without retaining its session owner.
pub(crate) struct Login {
    output: crate::output::Output, // Invocation's shared diagnostic stream
    interactive: bool,             // Whether browser login is permitted
}

impl Login {
    /// Captures CLI policy without looking up credentials or contacting a host.
    pub fn new(context: &Context) -> Self {
        Self {
            output: context.output.clone(),
            interactive: context.interactive(),
        }
    }
}

impl darkbio_connect::CloudAuth for Login {
    /// Reads only credentials belonging to a known internal API host.
    fn headers(&self, origin: &str, deadline: Instant) -> HeaderMap {
        if !matches!(
            origin,
            "https://api.darkbio.dev" | "https://api.darkbio.xyz"
        ) {
            return HeaderMap::new();
        }
        token(
            Command::new("cloudflared").args(["access", "token", "--app", origin]),
            deadline,
        )
        .ok()
        .and_then(|token| headers(&token).ok())
        .unwrap_or_default()
    }

    /// Recognizes Access separately from the cloud's device proof refusal.
    fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool {
        required(origin, status, headers)
    }

    /// Leaves browser interaction out of noninteractive commands and the connector.
    fn login(&self, origin: &str, deadline: Option<Instant>) -> Result<HeaderMap, String> {
        if !matches!(
            origin,
            "https://api.darkbio.dev" | "https://api.darkbio.xyz"
        ) {
            return Err("cloud access refused for an unsupported host".into());
        }
        let window = Instant::now() + Duration::from_secs(600);
        let token =
            self.authenticate(origin, deadline.map_or(window, |bound| bound.min(window)))?;
        headers(&token)
    }
}

/// Builds an authentication header without exposing invalid credentials in errors.
fn headers(token: &str) -> Result<HeaderMap, String> {
    let mut value =
        HeaderValue::from_str(token).map_err(|_| "invalid cloud credentials".to_owned())?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert("cf-access-token", value);
    Ok(headers)
}

impl Login {
    /// Starts one browser login under the supplied human or absolute window.
    fn authenticate(&self, origin: &str, deadline: Instant) -> Result<String, String> {
        if !self.interactive {
            return Err(format!("access to {origin} requires login"));
        }
        self.output.event(
            "note",
            format!("opening the browser to sign in to {origin}"),
        );
        token(
            Command::new("cloudflared").args(["access", "login", "--app", origin]),
            deadline,
        )
        .map_err(|error| error.to_string())
    }
}

/// Asks cloudflared for a cached application token, treating failure as no credentials.
pub(crate) fn cached(context: &Context, origin: &str) -> Option<String> {
    token(
        Command::new("cloudflared").args(["access", "token", "--app", origin]),
        context.deadline(),
    )
    .ok()
}

/// Starts browser login only when stdin prompts are permitted, under a separate
/// human login window. Noninteractive callers receive the manual login command.
pub(crate) fn authenticate(context: &Context, origin: &str) -> Result<String, Error> {
    Login::new(context)
        .authenticate(origin, Instant::now() + Duration::from_secs(600))
        .map_err(|err| {
            Error::new(4, "login-required", err)
                .hint(format!("run `cloudflared access login --app {origin}`"))
        })
}

/// Recognizes this tenant's login redirects and HTML refusals on internal hosts.
/// Cloud proof refusals remain plain responses and never start browser login.
pub(crate) fn required(origin: &str, status: StatusCode, headers: &HeaderMap) -> bool {
    if !matches!(
        origin,
        "https://api.darkbio.dev"
            | "https://api.darkbio.xyz"
            | "https://pkg.darkbio.dev"
            | "https://pkg.darkbio.xyz"
    ) {
        return false;
    }
    if status.is_redirection()
        && headers
            .get("location")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|redirect| challenge(origin, redirect))
    {
        return true;
    }
    matches!(status.as_u16(), 401 | 403)
        && headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("text/html")
            })
}

/// Matches the selected host's login path without following or trusting its query.
fn challenge(origin: &str, redirect: &str) -> bool {
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

/// Requires a positive remaining helper budget before spawning, polling or receiving.
fn remaining(deadline: Instant) -> Result<Duration, ConnectError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(ConnectError::Timeout)
}

/// Owned helper process, killed and reaped on every exit path.
struct Child(std::process::Child);

impl Drop for Child {
    /// Reaps the helper even when output collection, validation or the deadline failed.
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_refusals_do_not_include_device_proof_errors() {
        for origin in [
            "https://api.darkbio.dev",
            "https://api.darkbio.xyz",
            "https://pkg.darkbio.dev",
            "https://pkg.darkbio.xyz",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "location",
                format!(
                    "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/{}?redirect_url=/",
                    origin.trim_start_matches("https://")
                )
                .parse()
                .unwrap(),
            );
            assert!(required(origin, StatusCode::FOUND, &headers));
            headers.clear();
            for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
                assert!(!required(origin, status, &headers));
                headers.insert("content-type", "application/json".parse().unwrap());
                assert!(!required(origin, status, &headers));
                headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
                assert!(required(origin, status, &headers));
                headers.clear();
            }
        }
        let headers = HeaderMap::from_iter([(
            "content-type".parse().unwrap(),
            "text/html".parse().unwrap(),
        )]);
        for origin in [
            "https://api.dark.bio",
            "https://pkg.dark.bio",
            "https://api.darkbio.dev.evil.invalid",
            "http://api.darkbio.dev",
        ] {
            assert!(!required(origin, StatusCode::FORBIDDEN, &headers));
        }
    }

    #[test]
    fn noninteractive_login_returns_an_action_without_starting_a_helper() {
        use clap::Parser;
        use darkbio_connect::CloudAuth;
        let options = crate::args::Cli::parse_from(["ark", "--no-input"]).options;
        let login = Login {
            output: crate::output::Output::new(&options),
            interactive: false,
        };
        let message = login.login("https://api.darkbio.dev", None).unwrap_err();
        let error = Error::from(ConnectError::CloudAuth {
            origin: "https://api.darkbio.dev".into(),
            message,
        });
        assert_eq!((error.class, error.code), (4, "login-required"));
        assert_eq!(
            error.hints,
            ["run `cloudflared access login --app https://api.darkbio.dev`"]
        );
        assert!(
            login
                .headers("https://api.dark.bio", Instant::now())
                .is_empty()
        );
    }

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
