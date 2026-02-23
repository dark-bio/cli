// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use std::env;

fn main() {
    // Use vendored protoc so no system dependency is needed.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    // Safety: build scripts are single-threaded, so mutating the environment is safe.
    unsafe { env::set_var("PROTOC", &protoc) };

    // Compile the wire protocol protobuf from the firmware source tree.
    // TODO: replace hard-coded relative path with something more robust.
    prost_build::compile_protos(
        &["../firmware/arkos/apps/arkos-core/src/wire.proto"],
        &["../firmware/arkos/apps/arkos-core/src/"],
    )
    .expect("failed to compile wire.proto");
}
