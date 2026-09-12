// ark: command line for Dark Bio Arks
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Winget manifests generated from the released Windows executable.

use clap::Parser;
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

type Error = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(about = "Generate winget manifests from a released Windows executable")]
struct Args {
    /// Released Cargo version, without the v prefix
    #[arg(value_parser = parse_version)]
    version: Version,
    /// Released Windows amd64 executable
    binary: PathBuf,
    /// Directory for the three manifests
    #[arg(long)]
    output: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match generate(&args) {
        Ok(()) => {
            println!("{}", args.output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_version(value: &str) -> Result<Version, String> {
    let version = Version::parse(value).map_err(|error| error.to_string())?;
    if !version.build.is_empty() {
        return Err("release version must not contain build metadata".into());
    }
    Ok(version)
}

fn generate(args: &Args) -> Result<(), Error> {
    let binary = format!("ark-{}-windows-amd64.exe", args.version);
    if args.binary.file_name().and_then(|name| name.to_str()) != Some(binary.as_str()) {
        return Err(format!("expected {binary}").into());
    }
    let mut hash = Sha256::new();
    io::copy(&mut File::open(&args.binary)?, &mut hash)?;
    let digest = hex::encode_upper(hash.finalize());
    let version = serde_json::to_string(&args.version.to_string())?;
    let base = format!("PackageIdentifier: DarkBio.Ark\nPackageVersion: {version}\n");
    let root = format!(
        "https://github.com/dark-bio/cli/releases/download/v{}",
        args.version
    );
    let manifests = [
        ("", "version", "DefaultLocale: en-US\n".to_string()),
        (
            ".locale.en-US",
            "defaultLocale",
            "\
PackageLocale: en-US
Publisher: Dark Bio AG
PublisherUrl: https://dark.bio
PackageName: Ark
PackageUrl: https://github.com/dark-bio/cli
License: BSD-3-Clause
LicenseUrl: https://github.com/dark-bio/cli/blob/main/LICENSE
ShortDescription: Command line for Dark Bio Arks
"
            .to_string(),
        ),
        (
            ".installer",
            "installer",
            format!(
                "\
InstallerType: portable
Commands:
- ark
Installers:
- Architecture: x64
  InstallerUrl: {root}/{binary}
  InstallerSha256: {digest}
"
            ),
        ),
    ];
    fs::create_dir_all(&args.output)?;
    for (suffix, kind, content) in manifests {
        fs::write(
            args.output.join(format!("DarkBio.Ark{suffix}.yaml")),
            format!("{base}{content}ManifestType: {kind}\nManifestVersion: 1.9.0\n"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions() {
        for version in ["0.1.0", "0.1.0-rc.1"] {
            assert!(parse_version(version).is_ok());
        }
        for version in [
            "v0.1.0",
            "0.1",
            "0.1.0-",
            "0.1.0+local",
            "0.1.0/../../other",
        ] {
            assert!(parse_version(version).is_err(), "{version}");
        }
    }
}
