// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Discovery of emulated Arks running on the host.
//! Connect through [`Device::connect`] to authenticate a discovered Ark.

mod registry;
mod ws;

pub(crate) use registry::Instance;
pub(crate) use ws::connect;

use crate::{Device, Error};

/// Lists emulated Arks published by local launchers, without connecting to them.
/// An absent launcher registry returns an empty list.
pub fn list() -> Result<Vec<Device>, Error> {
    Ok(registry::list()?
        .into_iter()
        .map(Device::emulator)
        .collect())
}
