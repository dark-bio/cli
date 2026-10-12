// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Stamps the version `ark --version` prints.
//!
//! The publish workflow sets `ARK_VERSION` for the binaries it builds, and a
//! crate from crates.io carries the `.cargo_vcs_info.json` that `cargo
//! publish` packs. Either keeps the published version. Any other build is a
//! local one, marked by a `-local` suffix and the `ark_local` cfg, which turns
//! the release lookup off.

use std::{env, path::Path};

/// Sets `ARK_BUILD_VERSION` for the crate, and `ark_local` for a local build.
fn main() {
    // Rerun when the stamp changes, and watch the VCS file only where it
    // exists, since cargo treats a missing watched file as always changed
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=ARK_VERSION");
    println!("cargo::rustc-check-cfg=cfg(ark_local)");
    let directory = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the package directory");
    let packaged = Path::new(&directory).join(".cargo_vcs_info.json").is_file();
    if packaged {
        println!("cargo::rerun-if-changed=.cargo_vcs_info.json");
    }

    // Take the workflow's stamp, then a published crate's own version, and
    // mark anything else as a local build
    let version = env::var("ARK_VERSION").unwrap_or_else(|_| {
        let version = env::var("CARGO_PKG_VERSION").expect("Cargo sets the package version");
        if packaged {
            version
        } else {
            println!("cargo::rustc-cfg=ark_local");
            format!("{version}-local")
        }
    });
    println!("cargo::rustc-env=ARK_BUILD_VERSION={version}");
}
