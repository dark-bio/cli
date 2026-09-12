// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Package metadata and update candidate selection.

use crate::error::Error;

use serde::Deserialize;

/// Validated published archive metadata, independent of the installed device state.
#[derive(Clone, Debug)]
pub(crate) struct Package {
    /// Full Ark version, including the build suffix.
    pub version: String,
    /// Publisher's human description of the build.
    pub summary: String,
    /// Publisher's timestamp text, rendered as a date when parseable.
    pub published: String,
    /// Nonzero encrypted archive length in bytes.
    pub size: u64,
    /// Decoded archive digest used for routing and end-to-end download integrity.
    pub sha256: [u8; 32],
}
impl Package {
    /// Retains only the metadata connect needs to authorize and verify the update.
    pub fn firmware(&self) -> darkbio_connect::Firmware {
        darkbio_connect::Firmware {
            version: self.version.clone(),
            size: self.size,
            sha256: self.sha256,
        }
    }
}

/// Classifies malformed package versions, hashes and routes as rejected input.
fn invalid(message: String) -> Error {
    Error::new(1, "invalid-version", message)
}

/// Ark versions carry three u16 components and a seven-character build suffix.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version {
    /// Major, minor and patch components compared numerically.
    numbers: [u16; 3],
    stable: bool, // A stable build follows develop at the same semantic version
    /// Seven-character suffix, breaking ties after semantic version and stability.
    commit: String,
}

impl Version {
    /// Whether the suffix names the mutable develop build instead of a commit.
    pub(super) fn is_develop(&self) -> bool {
        !self.stable
    }

    /// Requires three u16 components and either develop or seven hexadecimal characters.
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
        let invalid = || invalid(format!("invalid firmware version {value:?}"));
        let (version, commit) = value.split_once('-').ok_or_else(invalid)?;
        let mut parts = version.split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            let part = parts.next().ok_or_else(invalid)?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            *number = part.parse().map_err(|_| invalid())?;
        }
        if parts.next().is_some()
            || commit.len() != 7
            || (commit != "develop" && !commit.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid());
        }
        Ok(Self {
            numbers,
            stable: commit != "develop",
            commit: commit.to_owned(),
        })
    }
}

/// Unvalidated package index decoded before artifact routes are accepted.
#[derive(Deserialize)]
pub(super) struct Listing {
    /// Package family, required to be arkos before any artifact is used.
    package: String,
    /// Published entries awaiting version, length, hash and path checks.
    artifacts: Vec<Artifact>,
}

/// Raw JSON entry whose fields must agree before becoming a package.
#[derive(Deserialize)]
struct Artifact {
    /// Advertised full version used in both archive path and cloud authorization.
    version: String,
    /// Publisher's description retained for selection output.
    summary: String,
    /// Publication timestamp retained as supplied by the index.
    published: String,
    /// Advertised archive length, rejected when zero.
    size: u64,
    /// Hexadecimal SHA-256, decoded to exactly 32 bytes.
    sha256: String,
    /// Archive route that must equal the path derived from version and digest.
    path: String,
}

impl Listing {
    /// Validate routing before any archive or device access. Archive paths must
    /// name the same version and hash as the cloud access-key request.
    pub(super) fn firmwares(self) -> Result<Vec<Package>, Error> {
        if self.package != "arkos" {
            return Err(invalid("package listing is not arkos".into()));
        }
        let mut firmwares = Vec::new();
        for artifact in self.artifacts {
            let version = Version::parse(&artifact.version)?;
            let mut sha256 = [0; 32];
            hex::decode_to_slice(&artifact.sha256, &mut sha256)
                .map_err(|_| invalid("invalid firmware SHA-256".into()))?;
            let firmware = Package {
                version: artifact.version,
                summary: artifact.summary,
                published: artifact.published,
                size: artifact.size,
                sha256,
            };
            if firmware.size == 0 || artifact.path.trim_start_matches('/') != path(&firmware) {
                return Err(invalid("invalid firmware size or archive path".into()));
            }
            firmwares.push((version, firmware));
        }
        firmwares.sort_by(|(a, _), (b, _)| b.cmp(a));
        Ok(firmwares
            .into_iter()
            .map(|(_, firmware)| firmware)
            .collect())
    }
}

/// Derives the canonical archive route from the validated version and digest.
pub(super) fn path(firmware: &Package) -> String {
    format!(
        "imgs/arkos-{}-{}.arch",
        firmware.version,
        hex::encode(firmware.sha256)
    )
}
/// Accepts a higher semantic version or any build replacing develop at the same version.
pub(super) fn candidate(firmware: &Package, installed: &str) -> Result<bool, Error> {
    let current = Version::parse(installed)?;
    let proposed = Version::parse(&firmware.version)?;
    Ok(proposed.numbers > current.numbers
        || (proposed.numbers == current.numbers && !current.stable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(version: &str) -> Package {
        Package {
            version: version.into(),
            summary: String::new(),
            published: "2026-01-01T00:00:00Z".into(),
            size: 1,
            sha256: [42; 32],
        }
    }
    fn listing(package: &Package) -> serde_json::Value {
        json!({"package":"arkos","artifacts":[{"version":package.version,"summary":package.summary,
            "published":package.published,"size":package.size,"sha256":hex::encode(package.sha256),"path":path(package)}]})
    }

    #[test]
    fn package_paths_and_hashes_must_agree() {
        let package = package("2.0.0-1234567");
        for mutate in [
            |value: &mut serde_json::Value| value["package"] = json!("foreign"),
            |value: &mut serde_json::Value| {
                value["artifacts"][0]["version"] = json!("1.2.3.4-develop")
            },
            |value: &mut serde_json::Value| value["artifacts"][0]["sha256"] = json!("aa"),
            |value: &mut serde_json::Value| value["artifacts"][0]["size"] = json!(0),
            |value: &mut serde_json::Value| {
                value["artifacts"][0]["path"] = json!("https://foreign.invalid/firmware")
            },
        ] {
            let mut value = listing(&package);
            mutate(&mut value);
            assert!(
                serde_json::from_value::<Listing>(value)
                    .unwrap()
                    .firmwares()
                    .is_err()
            );
        }
    }

    #[test]
    fn candidates_follow_semantic_versions_and_replace_develop_builds() {
        let mut value = listing(&package("2.0.0-1234567"));
        for version in ["1.0.0-develop", "3.0.0-develop", "3.0.0-1234567"] {
            value["artifacts"]
                .as_array_mut()
                .unwrap()
                .push(listing(&package(version))["artifacts"][0].clone());
        }
        let sorted = serde_json::from_value::<Listing>(value)
            .unwrap()
            .firmwares()
            .unwrap();
        assert_eq!(
            sorted
                .iter()
                .map(|package| package.version.as_str())
                .collect::<Vec<_>>(),
            [
                "3.0.0-1234567",
                "3.0.0-develop",
                "2.0.0-1234567",
                "1.0.0-develop"
            ]
        );
        let proposed = package("2.0.0-1234567");
        assert!(candidate(&proposed, "1.0.0-fffffff").unwrap());
        assert!(!candidate(&proposed, "2.0.0-0000000").unwrap());
        assert!(!candidate(&proposed, "3.0.0-develop").unwrap());
        assert!(candidate(&proposed, "2.0.0-develop").unwrap());
        assert!(candidate(&proposed, "not-a-version").is_err());
        // Explicit versions may be a downgrade or reinstallation; the Ark decides.
        assert_eq!(
            super::super::select(&sorted, Some("1.0.0-develop"), "3.0.0-1234567")
                .unwrap()
                .unwrap()
                .version,
            "1.0.0-develop"
        );
    }
}
