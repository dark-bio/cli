// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Build version from the publish stamp, packaged source or local checkout.

use std::{env, path::Path};

/// Preserves published versions and marks other builds as local.
fn main() {
    // A missing watched file is always stale, so watch VCS metadata only in packages
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=ARK_VERSION");
    println!("cargo::rustc-check-cfg=cfg(ark_local)");
    let directory = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the package directory");
    let packaged = Path::new(&directory).join(".cargo_vcs_info.json").is_file();
    if packaged {
        println!("cargo::rerun-if-changed=.cargo_vcs_info.json");
    }

    // The workflow stamps binaries; Cargo includes VCS metadata in published crates
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
