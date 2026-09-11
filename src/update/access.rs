// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

//! Browser login for development package hosts protected by Cloudflare Access.

use console::style;
use darkbio_connect::Error;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

type Header = (String, String);

/// Credentials live with the update's client and its clones. cloudflared owns
/// persistent storage and renewal; the CLI only retains the current header.
pub(super) fn authenticate()
-> impl Fn(&str, Option<&str>, Instant) -> Result<Option<Header>, Error> + Send + Sync {
    let cached = Mutex::new(None::<(String, String)>);
    move |origin, redirect, deadline| {
        if !protected(origin) {
            return Ok(None);
        }
        let mut cached = cached
            .try_lock()
            .map_err(|_| Error::Cloud("package login is already in progress".into()))?;
        if let Some(redirect) = redirect {
            if !challenge(origin, redirect) {
                return Ok(None);
            }
            eprintln!(
                "{}",
                style(format!("Authenticating to {origin} with cloudflared…")).dim()
            );
            let token = login(
                Command::new("cloudflared").args(["access", "login", "--app", origin]),
                deadline,
            )?;
            *cached = Some((origin.to_owned(), token));
        }
        Ok(cached
            .as_ref()
            .filter(|(host, _)| host == origin)
            .map(|(_, token)| ("cf-access-token".into(), token.clone())))
    }
}

fn protected(origin: &str) -> bool {
    match origin {
        #[cfg(feature = "develop")]
        "https://pkg.darkbio.dev" => true,
        #[cfg(feature = "staging")]
        "https://pkg.darkbio.xyz" => true,
        _ => false,
    }
}

/// The redirect identifies the challenge; cloudflared receives the original
/// package origin, so the server cannot choose where the CLI authenticates.
fn challenge(origin: &str, redirect: &str) -> bool {
    let Some(host) = origin.strip_prefix("https://") else {
        return false;
    };
    redirect.split(['?', '#']).next()
        == Some(
            format!("https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/{host}").as_str(),
        )
}

/// Captures only the token on stdout while login instructions reach stderr.
/// Expiration kills and reaps the helper, including while its output is blocked.
fn login(command: &mut Command, deadline: Instant) -> Result<String, Error> {
    remaining(deadline)?;
    let mut child = Child(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Error::Cloud(
                        "Cloudflare Access requires cloudflared; install it and retry the command"
                            .into(),
                    )
                } else {
                    Error::Cloud(format!("could not start cloudflared: {error}"))
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
        .map_err(Error::Worker)?;
    let status = loop {
        let remaining = remaining(deadline)?;
        if let Some(status) = child
            .0
            .try_wait()
            .map_err(|error| Error::Cloud(format!("could not wait for cloudflared: {error}")))?
        {
            break status;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    if !status.success() {
        return Err(Error::Cloud(format!("cloudflared login failed ({status})")));
    }
    let output = recv
        .recv_timeout(remaining(deadline)?)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::Timeout,
            mpsc::RecvTimeoutError::Disconnected => {
                Error::Cloud("cloudflared output reader stopped".into())
            }
        })?
        .map_err(|error| Error::Cloud(format!("could not read cloudflared token: {error}")))?;
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
        return Err(Error::Cloud(
            "cloudflared returned an invalid application token".into(),
        ));
    }
    Ok(token.to_owned())
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(Error::Timeout)
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

    /// Only enabled development hosts can invoke login. Redirects to other
    /// tenants, applications or origins never cause a helper to be launched.
    #[test]
    fn test_scope() {
        assert_eq!(
            protected("https://pkg.darkbio.dev"),
            cfg!(feature = "develop")
        );
        assert_eq!(
            protected("https://pkg.darkbio.xyz"),
            cfg!(feature = "staging")
        );
        let auth = authenticate();
        let deadline = Instant::now() + Duration::from_secs(1);
        for origin in [
            "https://pkg.dark.bio",
            "https://api.darkbio.dev/v1",
            "https://foreign.invalid",
            "http://pkg.darkbio.dev",
        ] {
            assert!(
                auth(
                    origin,
                    Some(
                        "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.dev"
                    ),
                    deadline
                )
                .unwrap()
                .is_none()
            );
        }
        for redirect in [
            "https://foreign.invalid/cdn-cgi/access/login/pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com.evil.invalid/cdn-cgi/access/login/pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.xyz",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.dev/extra",
        ] {
            assert!(
                auth("https://pkg.darkbio.dev", Some(redirect), deadline)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(challenge(
            "https://pkg.darkbio.dev",
            "https://darkbio.cloudflareaccess.com/cdn-cgi/access/login/pkg.darkbio.dev?redirect_url=%2Fimgs%2Farkos.pkgs"
        ));
    }

    /// Helper failures and malformed stdout are reported without including the
    /// token. The tests use a local child instead of opening a browser.
    #[cfg(unix)]
    #[test]
    fn test_login() {
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            login(
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
            let error = login(Command::new("sh").args(["-c", script]), deadline).unwrap_err();
            assert!(!error.to_string().contains("private-token"));
        }
        let error = login(
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
            login(
                &mut Command::new("/nonexistent/ark-test-cloudflared"),
                Instant::now()
            ),
            Err(Error::Timeout)
        ));
        let start = Instant::now();
        assert!(matches!(
            login(
                Command::new("sh").args(["-c", "exec sleep 5"]),
                start + Duration::from_millis(50)
            ),
            Err(Error::Timeout)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
