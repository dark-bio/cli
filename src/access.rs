// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Cloudflare Access login for internal API and package hosts.

use crate::{context::Context, error::Error};
use darkbio_clock::{Clock, crossbeam_channel};
use darkbio_connect::Error as ConnectError;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use ureq::http::{HeaderMap, HeaderValue, StatusCode};

/// Pace of the polls that observe a running helper's exit.
const POLL: Duration = Duration::from_millis(10);

/// Prompt policy copied into a connection without retaining its session owner.
pub(crate) struct Login {
    /// Clock of the connection, which measures the deadlines it passes in.
    clock: Clock,
    /// Shared diagnostic stream of the invocation.
    output: crate::output::Output,
    /// Whether a browser login may start.
    interactive: bool,
}

impl Login {
    /// Captures the invocation's prompt policy for a connection, without
    /// looking up credentials or contacting a host.
    ///
    /// The connection's clock measures the deadlines it passes in.
    pub fn new(context: &Context, clock: Clock) -> Self {
        Self {
            clock,
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
            &self.clock,
            deadline,
        )
        .ok()
        .and_then(|token| headers(&token).ok())
        .unwrap_or_default()
    }

    /// Recognizes a Cloudflare Access refusal, as [`required`] defines it.
    fn rejected(&self, origin: &str, status: StatusCode, headers: &HeaderMap) -> bool {
        required(origin, status, headers)
    }

    /// Runs a browser login for an internal API host, only when the invocation
    /// allows prompts.
    ///
    /// The login window is 600 s on the connection's clock, cut short by an
    /// earlier caller deadline.
    fn login(&self, origin: &str, deadline: Option<Instant>) -> Result<HeaderMap, String> {
        if !matches!(
            origin,
            "https://api.darkbio.dev" | "https://api.darkbio.xyz"
        ) {
            return Err("cloud access refused for an unsupported host".into());
        }
        let window = self.clock.now() + Duration::from_secs(600);
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
    /// Runs one browser login through cloudflared, which must end by `deadline`.
    ///
    /// A noninteractive invocation fails at once, without starting the helper.
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
            &self.clock,
            deadline,
        )
        .map_err(|error| error.to_string())
    }
}

/// Asks cloudflared for a cached application token, treating failure as no
/// credentials.
///
/// The lookup gets one command budget on the connection's clock.
pub(crate) fn cached(context: &Context, clock: &Clock, origin: &str) -> Option<String> {
    token(
        Command::new("cloudflared").args(["access", "token", "--app", origin]),
        clock,
        context.deadline(clock),
    )
    .ok()
}

/// Runs a browser login for `origin` when prompts are permitted, within a 600 s
/// window on the connection's clock.
///
/// Any failure, a noninteractive invocation included, returns a
/// `login-required` error that hints at the manual login command.
pub(crate) fn authenticate(
    context: &Context,
    clock: &Clock,
    origin: &str,
) -> Result<String, Error> {
    Login::new(context, clock.clone())
        .authenticate(origin, clock.now() + Duration::from_secs(600))
        .map_err(|err| {
            Error::new(4, "login-required", err)
                .hint(format!("run `cloudflared access login --app {origin}`"))
        })
}

/// Checks whether a response from an internal host is a Cloudflare Access
/// refusal.
///
/// A refusal is a redirect to this tenant's login page for the host, or a 401
/// or 403 status with an HTML body. Cloud proof refusals remain plain
/// responses and never start a browser login.
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

/// Runs a helper command and returns the application token it prints, never
/// forwarding its output to the terminal.
///
/// Expiration kills and reaps the helper, including while its output is blocked.
/// The helper's exit and output arrive as events, which the deadline bounds on
/// the clock. Output that is not a well-formed token fails without being echoed.
fn token(command: &mut Command, clock: &Clock, deadline: Instant) -> Result<String, ConnectError> {
    // Start the helper with only its stdout connected, unless the time is up
    remaining(clock, deadline)?;
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

    // Read the output on a thread of its own, stopping one byte past 64 KiB
    let stdout = child.0.stdout.take().expect("piped helper stdout");
    let (send, output) = crossbeam_channel::unbounded();
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

    // Watch the helper on a thread of its own, which reports its exit
    let (report, exited) = crossbeam_channel::bounded(1);
    let (abandon, abandoned) = crossbeam_channel::bounded(0);
    let watcher = thread::Builder::new()
        .name("access-helper".into())
        .spawn(move || watch(child, report, abandoned))
        .map_err(ConnectError::Worker)?;

    // Wait for the exit and the output, then stop the watcher, which kills and
    // reaps a helper that still runs
    let result = settle(clock, deadline, &exited, &output);
    drop(abandon);
    let _ = watcher.join();
    let output = result?;

    // Accept at most 64 KiB, holding three nonempty dot-separated parts of
    // URL-safe base64
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

/// Waits until the deadline for the helper's exit and then for its output.
///
/// A failed helper is reported by its exit status, never by its output.
fn settle(
    clock: &Clock,
    deadline: Instant,
    exited: &crossbeam_channel::Receiver<io::Result<ExitStatus>>,
    output: &crossbeam_channel::Receiver<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, ConnectError> {
    // The exit comes first, since a failed helper's output is never read
    remaining(clock, deadline)?;
    let status = clock
        .recv_deadline(exited, deadline)
        .map_err(|error| match error {
            crossbeam_channel::RecvTimeoutError::Timeout => ConnectError::Timeout,
            crossbeam_channel::RecvTimeoutError::Disconnected => {
                ConnectError::Cloud("cloudflared watcher stopped".into())
            }
        })?
        .map_err(|error| ConnectError::Cloud(format!("could not wait for cloudflared: {error}")))?;
    if !status.success() {
        return Err(ConnectError::Cloud(format!(
            "cloudflared login failed ({status})"
        )));
    }
    // The output follows within the same deadline
    remaining(clock, deadline)?;
    clock
        .recv_deadline(output, deadline)
        .map_err(|error| match error {
            crossbeam_channel::RecvTimeoutError::Timeout => ConnectError::Timeout,
            crossbeam_channel::RecvTimeoutError::Disconnected => {
                ConnectError::Cloud("cloudflared output reader stopped".into())
            }
        })?
        .map_err(|error| ConnectError::Cloud(format!("could not read cloudflared token: {error}")))
}

/// Polls the helper until it exits and reports the exit.
///
/// A waiter that gives up disconnects `abandoned`, which ends the polls at
/// once. Dropping the helper kills and reaps it either way.
#[expect(
    clippy::disallowed_methods,
    reason = "try_wait observes the helper's exit, and real time only paces its polls"
)]
fn watch(
    mut child: Child,
    report: crossbeam_channel::Sender<io::Result<ExitStatus>>,
    abandoned: crossbeam_channel::Receiver<()>,
) {
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) => {
                let _ = report.send(Ok(status));
                return;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = report.send(Err(error));
                return;
            }
        }
        if abandoned.recv_timeout(POLL) != Err(crossbeam_channel::RecvTimeoutError::Timeout) {
            return;
        }
    }
}

/// Returns the time left before `deadline` on the clock, failing with a timeout
/// when none is left.
///
/// Helper runs check it before spawning or receiving.
fn remaining(clock: &Clock, deadline: Instant) -> Result<Duration, ConnectError> {
    deadline
        .checked_duration_since(clock.now())
        .filter(|duration| !duration.is_zero())
        .ok_or(ConnectError::Timeout)
}

/// Owned helper process, killed and reaped on every exit path.
struct Child(std::process::Child);

impl Drop for Child {
    /// Kills and reaps the helper, even when output collection, validation or
    /// the deadline failed.
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Tests of the Access refusal checks and the cloudflared helper runs.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::wait_deadline;
    use darkbio_clock::TestClock;

    /// Checks that login redirects and HTML refusals on internal hosts count
    /// as Access refusals, while JSON refusals and other hosts do not.
    #[test]
    fn access_refusals_do_not_include_device_proof_errors() {
        // On each internal host, a login redirect and an HTML refusal count, but
        // a bare or JSON refusal does not
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

        // Public, lookalike and plain HTTP hosts never count, even with HTML
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

    /// Checks that a noninteractive login fails with the manual login command
    /// as its hint, and a public host gets no headers.
    #[test]
    fn noninteractive_login_returns_an_action_without_starting_a_helper() {
        use clap::Parser;
        use darkbio_connect::CloudAuth;

        // A noninteractive login refuses, and the refusal maps to a
        // login-required error with the manual command
        let options = crate::args::Cli::parse_from(["ark", "--no-input"]).options;
        let clock = TestClock::new().clock();
        let login = Login {
            clock: clock.clone(),
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

        // A public host gets no headers, so no helper starts for it
        assert!(
            login
                .headers("https://api.dark.bio", clock.now())
                .is_empty()
        );
    }

    /// Checks that only the tenant's login page for the exact host counts as a
    /// challenge.
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

    /// Checks that a helper's token comes back, while helper failures and
    /// malformed output never echo it.
    ///
    /// A local shell stands in for cloudflared, so no browser opens.
    #[cfg(unix)]
    #[test]
    fn test_token() {
        // A well-formed token comes back without its line ending
        let clock = TestClock::new().clock();
        let deadline = clock.now() + Duration::from_secs(5);
        assert_eq!(
            token(
                Command::new("sh").args(["-c", "printf 'e30.e30.signature\\n'"]),
                &clock,
                deadline
            )
            .unwrap(),
            "e30.e30.signature"
        );

        // Empty, malformed, failed and oversized output fails without the token
        for script in [
            "exit 0",
            "printf 'private-token'",
            "printf 'e30.e30.private-token'; exit 1",
            "head -c 70000 /dev/zero",
        ] {
            let error =
                token(Command::new("sh").args(["-c", script]), &clock, deadline).unwrap_err();
            assert!(!error.to_string().contains("private-token"));
        }

        // A missing helper asks for cloudflared to be installed
        let error = token(
            &mut Command::new("/nonexistent/ark-test-cloudflared"),
            &clock,
            deadline,
        )
        .unwrap_err();
        assert!(error.to_string().contains("install it"));
    }

    /// Checks that an expired deadline starts no helper, and a reached deadline
    /// ends a running one instead of waiting out its own login timeout.
    #[cfg(unix)]
    #[test]
    fn test_deadline() {
        // An expired deadline refuses before starting anything
        let mut tester = TestClock::new();
        let clock = tester.clock();
        assert!(matches!(
            token(
                &mut Command::new("/nonexistent/ark-test-cloudflared"),
                &clock,
                clock.now()
            ),
            Err(ConnectError::Timeout)
        ));

        // A helper announces its start through a named pipe, and would then
        // run for an hour
        let pipe = std::env::temp_dir().join(format!("ark-access-test-{}", std::process::id()));
        assert!(
            Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .unwrap()
                .success()
        );
        let script = format!("echo started > '{}'; exec sleep 3600", pipe.display());
        let deadline = clock.now() + Duration::from_millis(50);
        let waiting = thread::spawn(move || {
            token(Command::new("sh").args(["-c", &script]), &clock, deadline)
        });
        std::fs::read(&pipe).unwrap();
        std::fs::remove_file(&pipe).unwrap();

        // Reaching the deadline during the wait for the exit ends it with a
        // timeout. The helper is killed and reaped before the call returns.
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        assert!(matches!(
            waiting.join().unwrap(),
            Err(ConnectError::Timeout)
        ));
    }

    /// Builds the exit status of a helper that ended with `code`.
    #[cfg(unix)]
    fn exited(code: i32) -> ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    /// Builds the exit status of a helper that ended with `code`.
    #[cfg(windows)]
    fn exited(code: i32) -> ExitStatus {
        std::os::windows::process::ExitStatusExt::from_raw(code as u32)
    }

    /// Checks that the helper's exit and then its output settle the wait, while
    /// a failed exit or the deadline ends it.
    #[test]
    fn test_settle() {
        // Settles one helper's events on a thread, under a deadline a second
        // away. An exit given here is queued before the wait starts.
        let mut tester = TestClock::new();
        let settling = |tester: &TestClock, exit: Option<io::Result<ExitStatus>>| {
            let clock = tester.clock();
            let deadline = clock.now() + Duration::from_secs(1);
            let (exits, exited) = crossbeam_channel::unbounded();
            let (output, outputs) = crossbeam_channel::unbounded();
            if let Some(exit) = exit {
                exits.send(exit).unwrap();
            }
            let result = thread::spawn(move || settle(&clock, deadline, &exited, &outputs));
            (exits, output, deadline, result)
        };

        // The exit and the output arrive while the wait is on
        let (exit, output, deadline, result) = settling(&tester, None);
        wait_deadline(&tester, deadline);
        exit.send(Ok(exited(0))).unwrap();
        output.send(Ok(b"e30.e30.signature".to_vec())).unwrap();
        assert_eq!(result.join().unwrap().unwrap(), b"e30.e30.signature");

        // A failed exit fails the wait without the output queued ahead of it
        let (exit, output, _, result) = settling(&tester, None);
        output.send(Ok(b"e30.e30.private-token".to_vec())).unwrap();
        exit.send(Ok(exited(1))).unwrap();
        let error = result.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("login failed") && !error.contains("private-token"));

        // So does an exit the watcher could not observe
        let (exit, _output, _, result) = settling(&tester, None);
        exit.send(Err(io::Error::other("no such process"))).unwrap();
        assert!(
            result
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("could not wait")
        );

        // The deadline ends a wait for the exit, the only wait with a timer
        let (_exit, _output, deadline, result) = settling(&tester, None);
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        assert!(matches!(result.join().unwrap(), Err(ConnectError::Timeout)));

        // It also ends a wait for the output. The exit is queued first, so its
        // receive arms no timer and the one armed belongs to the output wait.
        let (_exit, _output, deadline, result) = settling(&tester, Some(Ok(exited(0))));
        tester.wait_timers(1);
        assert_eq!(tester.next_deadline(), Some(deadline));
        tester.advance_to(deadline);
        assert!(matches!(result.join().unwrap(), Err(ConnectError::Timeout)));
    }
}
