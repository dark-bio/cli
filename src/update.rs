// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Lookups of the newest published `ark` and the note that announces it.

use crate::output::Output;
use chrono::{DateTime, Utc};
use darkbio_clock::Clock;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Hidden sole argument that makes `ark` run the detached lookup and exit.
pub(crate) const ENTRY_POINT: &str = "__update";

/// Largest kept answer read from disk, in bytes.
const CACHE_LIMIT: u64 = 4 * 1024;
/// Largest development release response accepted from GitHub, in bytes.
const RESPONSE_LIMIT: u64 = 1024 * 1024;

/// Release channel of a build, distinguished by its version's prerelease field.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Channel {
    /// Stable releases, published from version tags.
    Release,
    /// Development builds, published as prereleases from every push to main.
    Develop,
}

impl Channel {
    /// Puts a version without a prerelease identifier on the release channel.
    pub fn for_version(version: &Version) -> Self {
        if version.pre.is_empty() {
            Self::Release
        } else {
            Self::Develop
        }
    }

    /// Names the channel in the detail of doctor's update check.
    pub fn description(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Develop => "development build",
        }
    }
}

/// The kept answer, stamped when its lookup started.
#[derive(Deserialize, Serialize)]
pub(crate) struct Answer {
    /// Channel the answer belongs to.
    pub channel: Channel,
    /// Start time of the last lookup, whether or not it succeeded.
    pub asked: DateTime<Utc>,
    /// Newest version found, absent until a lookup succeeds.
    pub newest: Option<Version>,
}

impl Answer {
    /// Reads the kept answer for one channel.
    ///
    /// An unreadable or malformed file, or an answer for the other channel,
    /// counts as no answer.
    pub fn read(directory: &Path, channel: Channel) -> Option<Self> {
        // Read at most 4 KiB, far more than an answer ever takes
        let file = File::open(directory.join("update.json")).ok()?;
        let mut bytes = Vec::new();
        file.take(CACHE_LIMIT).read_to_end(&mut bytes).ok()?;

        // Typed fields reject malformed versions and timestamps
        let answer: Self = serde_json::from_slice(&bytes).ok()?;
        (answer.channel == channel).then_some(answer)
    }

    /// Reports whether a lookup is due.
    ///
    /// It is when the answer is absent, belongs to the other channel, is stamped
    /// in the future, or is an hour old.
    pub fn stale(answer: Option<&Self>, channel: Channel, now: DateTime<Utc>) -> bool {
        answer.is_none_or(|answer| {
            answer.channel != channel
                || answer.asked > now
                || now.signed_duration_since(answer.asked) >= chrono::Duration::hours(1)
        })
    }

    /// Replaces the kept answer through a renamed temporary file, so a reader
    /// never sees half of it.
    pub fn write(&self, directory: &Path) -> io::Result<()> {
        // Serialize the answer as one line ending in a newline, and make sure
        // the cache directory exists
        let mut bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        bytes.push(b'\n');
        fs::create_dir_all(directory)?;

        // Rename only a completely written file over the kept answer
        let temporary = directory.join(format!(".update-{}.tmp", std::process::id()));
        let result = fs::write(&temporary, bytes)
            .and_then(|()| fs::rename(&temporary, directory.join("update.json")));
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Words the note while the kept version is newer than the running one.
    fn note(&self, running: &Version, hint: &str) -> Option<String> {
        let newest = self.newest.as_ref()?;
        newest
            .cmp_precedence(running)
            .is_gt()
            .then(|| format!("ark {newest} is available, this is {running}; {hint}"))
    }
}

/// Reports whether a nonempty `CI` turns off lookups and notes alike.
pub(crate) fn disabled() -> bool {
    std::env::var_os("CI").is_some_and(|value| !value.is_empty())
}

/// Parses the version stamped into this executable by Cargo or the publish workflow.
pub(crate) fn running() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is semver")
}

/// Prints the note from the kept answer and starts a background lookup when
/// one is due.
///
/// The lookup runs in a detached copy of `ark`.
pub(crate) fn start(output: &Output, now: DateTime<Utc>) {
    // Under CI nothing is read, printed or looked up
    if disabled() {
        return;
    }

    // Print the note from the kept answer, and go on only if a lookup is due
    let running = running();
    let channel = Channel::for_version(&running);
    let directory = crate::data::cache::directory();
    let answer = Answer::read(&directory, channel);
    if let Some(note) = answer
        .as_ref()
        .and_then(|answer| answer.note(&running, &hint(channel)))
    {
        output.event("note", note);
    }
    if !Answer::stale(answer.as_ref(), channel, now) {
        return;
    }

    // Stamp the attempt before asking, so a failing network asks once an hour.
    // A cache that cannot be written gets no lookup at all.
    if let Err(error) = claim(&directory, channel, now) {
        tracing::debug!("update claim could not be written: {}", error);
        return;
    }

    // Start the copy in its own process group with no streams, so a harness
    // waiting for this command's output never waits for the lookup
    let result = std::env::current_exe().and_then(|executable| {
        let mut command = Command::new(executable);
        command
            .arg(ENTRY_POINT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{
                CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
            };
            command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        command.spawn().map(drop)
    });
    if let Err(error) = result {
        tracing::debug!("update process could not be started: {}", error);
    }
}

/// Runs the lookup in the detached copy, which ends within 30 s whatever happens.
///
/// The copy opens no connection, so its clock is the real one.
pub(crate) fn run() {
    // Under CI the copy does nothing
    if disabled() {
        return;
    }
    let clock = Clock::real();
    let started = clock.now();
    let asked = DateTime::<Utc>::from(clock.system_time());

    // End the process at 30 s, and skip the lookup when nothing can enforce that
    if watchdog(started, Duration::from_secs(30)).is_err() {
        return;
    }

    // Keep a successful answer, stamped with this copy's start time
    let channel = Channel::for_version(&running());
    let _ = refresh(
        &crate::data::cache::directory(),
        channel,
        asked,
        Duration::from_secs(20),
    );
}

/// Ends the process once `limit` has passed since `started` on the real clock,
/// whatever the lookup is doing.
#[expect(
    clippy::disallowed_methods,
    reason = "the watchdog bounds the real lifetime of the detached lookup process"
)]
fn watchdog(started: Instant, limit: Duration) -> io::Result<()> {
    thread::Builder::new()
        .name("ark-update-watchdog".into())
        .spawn(move || {
            thread::sleep(limit.saturating_sub(started.elapsed()));
            std::process::exit(0);
        })
        .map(drop)
}

/// Stamps a new attempt, keeping the version last found on the same channel.
fn claim(directory: &Path, channel: Channel, now: DateTime<Utc>) -> io::Result<()> {
    Answer {
        channel,
        asked: now,
        newest: Answer::read(directory, channel).and_then(|answer| answer.newest),
    }
    .write(directory)
}

/// Looks up the newest version and keeps it.
///
/// A failed lookup leaves the kept answer as it was.
pub(crate) fn refresh(
    directory: &Path,
    channel: Channel,
    asked: DateTime<Utc>,
    timeout: Duration,
) -> Result<Version, &'static str> {
    // A failed lookup leaves the kept version and attempt time untouched
    let newest = lookup(channel, timeout)?;
    let answer = Answer {
        channel,
        asked,
        newest: Some(newest.clone()),
    };

    // A failed write still returns the version, since doctor shows it either way
    if let Err(error) = answer.write(directory) {
        tracing::debug!("update answer could not be written: {}", error);
    }
    Ok(newest)
}

/// Fetches only a parsed version; failure reasons never contain response text.
fn lookup(channel: Channel, timeout: Duration) -> Result<Version, &'static str> {
    let agent = crate::http::agent(timeout, 0);
    let result = match channel {
        Channel::Release => {
            // Inspect the redirect without following it or reading its body
            let response = agent
                .head("https://github.com/dark-bio/cli/releases/latest")
                .header("User-Agent", "ark")
                .call()
                .map_err(|error| {
                    tracing::debug!("update lookup failed: {}", error);
                    "GitHub could not be reached"
                })?;
            if !response.status().is_redirection() {
                Err("GitHub returned an unexpected status")
            } else {
                response
                    .headers()
                    .get("Location")
                    .and_then(|location| location.to_str().ok())
                    .ok_or("GitHub returned no release redirect")
                    .and_then(release)
            }
        }
        Channel::Develop => {
            // Bound the public release list before parsing any of its entries
            let mut response = agent
                .get("https://api.github.com/repos/dark-bio/cli/releases?per_page=10")
                .header("User-Agent", "ark")
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .map_err(|error| {
                    tracing::debug!("update lookup failed: {}", error);
                    "GitHub could not be reached"
                })?;
            if !response.status().is_success() {
                Err("GitHub returned an unexpected status")
            } else {
                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(RESPONSE_LIMIT)
                    .read_to_vec()
                    .map_err(|error| {
                        tracing::debug!("update lookup failed: {}", error);
                        "GitHub release list could not be read within 1 MiB"
                    })?;
                develop(&bytes)
            }
        }
    };

    // Log validation failures with their fixed texts, never response content
    if let Err(error) = result {
        tracing::debug!("update lookup failed: {}", error);
    }
    result
}

/// Accepts only a strict stable version at this repository's exact release URL.
fn release(location: &str) -> Result<Version, &'static str> {
    location
        .strip_prefix("https://github.com/dark-bio/cli/releases/tag/v")
        .and_then(|tag| Version::parse(tag).ok())
        .filter(|version| version.pre.is_empty())
        .ok_or("GitHub returned an invalid release redirect")
}

/// Selects the highest semantic version among published prerelease entries.
fn develop(bytes: &[u8]) -> Result<Version, &'static str> {
    /// Public release fields that version selection reads.
    #[derive(Deserialize)]
    struct Release {
        /// Version tag stamped by the publish workflow.
        tag_name: String,
        /// Whether the entry is an unpublished draft, which never announces a
        /// build.
        draft: bool,
        /// Whether the entry is a prerelease, since stable releases do not
        /// belong to the development channel.
        prerelease: bool,
    }

    // Decode the public fields without retaining unrelated response text
    let releases: Vec<Release> =
        serde_json::from_slice(bytes).map_err(|_| "GitHub returned an invalid release list")?;

    // Ignore unpublished entries and invalid tags before comparing semantic
    // precedence
    releases
        .into_iter()
        .filter(|release| !release.draft && release.prerelease)
        .filter_map(|release| {
            release
                .tag_name
                .strip_prefix('v')
                .and_then(|tag| Version::parse(tag).ok())
        })
        .max_by(Version::cmp_precedence)
        .ok_or("GitHub returned no development builds")
}

/// Words the upgrade advice for the way this executable was installed.
pub(crate) fn hint(channel: Channel) -> String {
    // Gather the paths that distinguish the install methods
    let executable = std::env::current_exe().ok();
    let home = directories::BaseDirs::new();
    let cargo_home = std::env::var_os("CARGO_HOME");

    // An install the tool cannot place gets the repository link
    executable
        .as_deref()
        .and_then(|executable| {
            upgrade(
                channel,
                executable,
                home.as_ref().map(|dirs| dirs.home_dir()),
                cargo_home.as_deref().map(Path::new),
            )
        })
        .map(|command| format!("upgrade with `{command}`"))
        .unwrap_or_else(|| "download it from https://github.com/dark-bio/cli".into())
}

/// Picks the upgrade command for an executable's location.
///
/// The installer and crates.io carry releases only, so a development build
/// gets a command only from Homebrew.
fn upgrade(
    channel: Channel,
    executable: &Path,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
) -> Option<&'static str> {
    // Follow Homebrew's link into its Cellar and match the formula
    let executable = executable.canonicalize().ok()?;
    let mut components = executable.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == "Cellar" {
            match components.next()?.as_os_str().to_str()? {
                "ark-cli" => return Some("brew update && brew upgrade ark-cli"),
                "ark-cli-dev" => return Some("brew update && brew upgrade ark-cli-dev"),
                _ => {}
            }
        }
    }

    // Past Homebrew, only a release build has an upgrade command
    if channel == Channel::Develop {
        return None;
    }

    // Compare canonical directories, since the home or bin directory may be a link
    let directory = executable.parent()?;
    if home
        .and_then(|home| home.join(".local/bin").canonicalize().ok())
        .as_deref()
        == Some(directory)
    {
        return Some(
            "curl -fsSL https://github.com/dark-bio/cli/releases/latest/download/ark-installer.sh | sh",
        );
    }
    let cargo = cargo_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".cargo")));
    if cargo
        .and_then(|cargo| cargo.join("bin").canonicalize().ok())
        .as_deref()
        == Some(directory)
    {
        return Some("cargo install darkbio-ark --locked");
    }
    None
}

/// Tests of the kept answer, the response parsing and the install detection.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counter that distinguishes temporary test directories without relying
    /// on timestamps.
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// Temporary test directory, removed with its cache and installation files
    /// on scope exit.
    struct Directory {
        /// Isolated root for one test's real filesystem operations.
        path: PathBuf,
    }

    impl Directory {
        /// Creates a process-specific directory without overwriting existing files.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ark-update-test-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for Directory {
        /// Cleans up even when an assertion unwinds the test.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A lookup is due after an hour, for a future stamp and for the other channel.
    #[test]
    fn test_cache_staleness_uses_the_claim_time_and_channel() {
        // Use a fixed instant so the boundary never depends on test runtime
        let directory = Directory::new();
        let now = "2026-09-25T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let cache = directory.path.join("cache");
        assert!(Answer::stale(
            Answer::read(&cache, Channel::Release).as_ref(),
            Channel::Release,
            now
        ));

        // Keep each case through the production writer and read it back
        for (case, asked, channel, stale) in [
            (
                "under an hour",
                "2026-09-25T11:00:00.001Z",
                Channel::Release,
                false,
            ),
            ("one hour", "2026-09-25T11:00:00Z", Channel::Release, true),
            ("future", "2026-09-25T12:00:00.001Z", Channel::Release, true),
            (
                "other channel",
                "2026-09-25T12:00:00Z",
                Channel::Develop,
                true,
            ),
        ] {
            let answer = Answer {
                channel,
                asked: asked.parse().unwrap(),
                newest: Some(Version::parse("0.3.7").unwrap()),
            };
            answer.write(&cache).unwrap();
            let kept = Answer::read(&cache, Channel::Release);
            assert_eq!(
                Answer::stale(kept.as_ref(), Channel::Release, now),
                stale,
                "{case}"
            );
        }

        // The last write leaves one file holding the documented shape
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.join("update.json")).unwrap()).unwrap();
        assert_eq!(
            stored,
            json!({"channel":"develop", "asked":"2026-09-25T12:00:00Z", "newest":"0.3.7"})
        );
        assert_eq!(fs::read_dir(cache).unwrap().count(), 1);
    }

    /// Malformed and unreadable answers never postpone a fresh lookup.
    #[test]
    fn test_invalid_cache_answers_are_stale() {
        // Keep the malformed fields close to the documented cache shape
        let directory = Directory::new();
        let now = "2026-09-25T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let path = directory.path.join("update.json");
        for (case, bytes) in [
            ("broken JSON", b"{".to_vec()),
            (
                "invalid time",
                br#"{"channel":"release","asked":"today","newest":"0.3.7"}"#.to_vec(),
            ),
            (
                "invalid version",
                br#"{"channel":"release","asked":"2026-09-25T12:00:00Z","newest":"latest"}"#
                    .to_vec(),
            ),
        ] {
            fs::write(&path, bytes).unwrap();
            let answer = Answer::read(&directory.path, Channel::Release);
            assert!(
                Answer::stale(answer.as_ref(), Channel::Release, now),
                "{case}"
            );
        }

        // A directory in place of the answer exercises an unreadable file portably
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(Answer::stale(
            Answer::read(&directory.path, Channel::Release).as_ref(),
            Channel::Release,
            now
        ));
    }

    /// Claims keep only the same channel's previous version and require a
    /// writable cache.
    #[test]
    fn test_claim_preserves_only_the_same_channels_previous_answer() {
        // Publish an expired answer through the same atomic writer used by the
        // worker
        let directory = Directory::new();
        let now = "2026-09-25T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        Answer {
            channel: Channel::Release,
            asked: "2026-09-25T10:00:00Z".parse().unwrap(),
            newest: Some(Version::parse("0.3.7").unwrap()),
        }
        .write(&directory.path)
        .unwrap();

        // A claim keeps the known version and records the new attempt time
        claim(&directory.path, Channel::Release, now).unwrap();
        let claimed = Answer::read(&directory.path, Channel::Release).unwrap();
        assert_eq!(claimed.asked.to_rfc3339(), "2026-09-25T12:00:00+00:00");
        assert_eq!(claimed.newest.unwrap().to_string(), "0.3.7");

        // A claim for the other channel drops the version found on this one
        claim(&directory.path, Channel::Develop, now).unwrap();
        let claimed = Answer::read(&directory.path, Channel::Develop).unwrap();
        assert!(claimed.newest.is_none());
        assert!(Answer::read(&directory.path, Channel::Release).is_none());

        // An unwritable cache path cannot produce a claim or start a request
        let file = directory.path.join("file");
        fs::write(&file, []).unwrap();
        assert!(claim(&file, Channel::Release, now).is_err());
    }

    /// Only a newer kept version produces the exact notice, regardless of claim age.
    #[test]
    fn test_note_requires_a_newer_kept_version() {
        // The old timestamp deliberately leaves the notice independent of freshness
        let mut answer = Answer {
            channel: Channel::Release,
            asked: "2026-01-01T00:00:00Z".parse().unwrap(),
            newest: None,
        };
        let running = Version::parse("0.3.6").unwrap();
        let hint = "upgrade with `brew update && brew upgrade ark-cli`";
        assert!(answer.note(&running, hint).is_none());
        for newest in ["0.3.5", "0.3.6", "0.3.6+different-build"] {
            answer.newest = Some(Version::parse(newest).unwrap());
            assert!(answer.note(&running, hint).is_none(), "{newest}");
        }

        // Both known and unknown installation methods keep the promised sentence
        answer.newest = Some(Version::parse("0.3.7").unwrap());
        assert_eq!(
            answer.note(&running, hint).unwrap(),
            "ark 0.3.7 is available, this is 0.3.6; upgrade with `brew update && brew upgrade ark-cli`"
        );
        assert_eq!(
            answer
                .note(&running, "download it from https://github.com/dark-bio/cli")
                .unwrap(),
            "ark 0.3.7 is available, this is 0.3.6; download it from https://github.com/dark-bio/cli"
        );

        // Numeric prerelease identifiers follow semantic precedence rather than
        // text order
        answer.channel = Channel::Develop;
        answer.newest = Some(Version::parse("0.3.6-dev.34").unwrap());
        assert_eq!(
            answer
                .note(
                    &Version::parse("0.3.6-dev.9").unwrap(),
                    "upgrade with `brew update && brew upgrade ark-cli-dev`"
                )
                .unwrap(),
            "ark 0.3.6-dev.34 is available, this is 0.3.6-dev.9; upgrade with `brew update && brew upgrade ark-cli-dev`"
        );
    }

    /// Stable redirects must name this repository and a strict release version.
    #[test]
    fn test_release_redirect_rejects_foreign_and_nonrelease_locations() {
        // Captured with curl -sI from releases/latest on 2026-09-25 (HTTP 302)
        assert_eq!(
            release("https://github.com/dark-bio/cli/releases/tag/v0.3.5").unwrap(),
            Version::parse("0.3.5").unwrap()
        );
        for location in [
            "https://example.com/dark-bio/cli/releases/tag/v0.3.5",
            "https://github.com/dark-bio/emulator/releases/tag/v0.3.5",
            "https://github.com/dark-bio/cli/releases/tag/v0.3.6-dev.34",
            "https://github.com/dark-bio/cli/releases/tag/v0.3",
            "https://github.com/dark-bio/cli/releases/tag/v0.3.5/extra",
            "https://github.com/dark-bio/cli/releases/tag/v0.3.5?next=bad",
            "https://github.com/dark-bio/cli/releases/tag/v0.3.5\n",
            "garbage",
        ] {
            assert!(release(location).is_err(), "{location:?}");
        }
    }

    /// Development selection ignores drafts and releases and does not depend on
    /// list order.
    #[test]
    fn test_development_selection_uses_the_highest_published_prerelease() {
        // Captured 2026-09-25 from https://api.github.com/repos/dark-bio/cli/releases?per_page=10,
        // with only unrelated object fields removed from this public response
        let bytes = br#"[
            {"tag_name":"v0.3.6-dev.34","draft":false,"prerelease":true},
            {"tag_name":"v0.3.5","draft":false,"prerelease":false},
            {"tag_name":"v0.3.5-dev.32","draft":false,"prerelease":true},
            {"tag_name":"v0.3.5-dev.31","draft":false,"prerelease":true},
            {"tag_name":"v0.3.4","draft":false,"prerelease":false},
            {"tag_name":"v0.3.4-dev.28","draft":false,"prerelease":true},
            {"tag_name":"v0.3.4-dev.27","draft":false,"prerelease":true},
            {"tag_name":"v0.3.4-dev.26","draft":false,"prerelease":true},
            {"tag_name":"v0.3.3","draft":false,"prerelease":false},
            {"tag_name":"v0.3.3-dev.23","draft":false,"prerelease":true}
        ]"#;
        assert_eq!(
            develop(bytes).unwrap(),
            Version::parse("0.3.6-dev.34").unwrap()
        );

        // Reverse the captured order so the winner is neither first nor assumed
        // latest
        let mut releases: Vec<serde_json::Value> = serde_json::from_slice(bytes).unwrap();
        releases.reverse();
        assert_eq!(
            develop(&serde_json::to_vec(&releases).unwrap()).unwrap(),
            Version::parse("0.3.6-dev.34").unwrap()
        );

        // Turning just the highest entry into a draft leaves a stable release
        // above the winner
        releases.last_mut().unwrap()["draft"] = json!(true);
        assert_eq!(
            develop(&serde_json::to_vec(&releases).unwrap()).unwrap(),
            Version::parse("0.3.5-dev.32").unwrap()
        );

        // Only drafts, invalid JSON or only invalid tags give no version
        for release in &mut releases {
            release["draft"] = json!(true);
        }
        assert!(develop(&serde_json::to_vec(&releases).unwrap()).is_err());
        assert!(develop(b"not JSON").is_err());
        assert!(develop(br#"[{"tag_name":"garbage","draft":false,"prerelease":true}]"#).is_err());
    }

    /// Install advice follows canonical paths and keeps development builds off
    /// release installers.
    #[test]
    fn test_upgrade_commands_follow_the_installation_layout() {
        // Create the installed files because detection resolves the executable
        // itself
        let directory = Directory::new();
        let home = directory.path.join("home");
        let cargo = directory.path.join("custom-cargo");
        for (path, cargo_home, release, develop) in [
            (
                "Cellar/ark-cli/0.3.6/bin/ark",
                None,
                Some("brew update && brew upgrade ark-cli"),
                Some("brew update && brew upgrade ark-cli"),
            ),
            (
                "Cellar/ark-cli-dev/0.3.6-dev.34/bin/ark",
                None,
                Some("brew update && brew upgrade ark-cli-dev"),
                Some("brew update && brew upgrade ark-cli-dev"),
            ),
            (
                "home/.local/bin/ark",
                None,
                Some(
                    "curl -fsSL https://github.com/dark-bio/cli/releases/latest/download/ark-installer.sh | sh",
                ),
                None,
            ),
            (
                "home/.cargo/bin/ark",
                None,
                Some("cargo install darkbio-ark --locked"),
                None,
            ),
            (
                "custom-cargo/bin/ark",
                Some(cargo.as_path()),
                Some("cargo install darkbio-ark --locked"),
                None,
            ),
            ("home/.cargo/bin/ark", Some(cargo.as_path()), None, None),
            ("custom-cargo/bin/ark", None, None, None),
            ("Cellar/ark-cli-extra/0.3.6/bin/ark", None, None, None),
            ("Cellarish/ark-cli/0.3.6/bin/ark", None, None, None),
            ("opt/bin/ark", None, None, None),
        ] {
            let executable = directory.path.join(path);
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, []).unwrap();
            assert_eq!(
                upgrade(Channel::Release, &executable, Some(&home), cargo_home),
                release,
                "{path}, release, {cargo_home:?}"
            );
            assert_eq!(
                upgrade(Channel::Develop, &executable, Some(&home), cargo_home),
                develop,
                "{path}, develop, {cargo_home:?}"
            );
        }

        // A normal Homebrew entry point resolves through its Cellar symlink
        #[cfg(unix)]
        {
            let link = directory.path.join("ark");
            std::os::unix::fs::symlink(directory.path.join("Cellar/ark-cli/0.3.6/bin/ark"), &link)
                .unwrap();
            assert_eq!(
                upgrade(Channel::Release, &link, Some(&home), None),
                Some("brew update && brew upgrade ark-cli")
            );
            let linked_home = directory.path.join("linked-home");
            std::os::unix::fs::symlink(&home, &linked_home).unwrap();
            assert_eq!(
                upgrade(
                    Channel::Release,
                    &home.join(".local/bin/ark"),
                    Some(&linked_home),
                    None
                ),
                Some(
                    "curl -fsSL https://github.com/dark-bio/cli/releases/latest/download/ark-installer.sh | sh"
                )
            );
        }
    }
}
