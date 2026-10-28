//! Generates the `dapr.proto.components.v1` Sandbox service (server and
//! client) and a file descriptor set for gRPC reflection.
//!
//! protoc: `$PROTOC` if set; otherwise the pinned protoc 34.1 at
//! `~/.local/protoc-34.1/bin/protoc` if it exists; otherwise `protoc` on PATH.

use std::env;
use std::path::PathBuf;

const PROTO_ROOT: &str = "proto";
const PROTOS: &[&str] = &[
    "proto/dapr/proto/components/v1/common.proto",
    "proto/dapr/proto/components/v1/sandbox.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=PROTOC");
    for p in PROTOS {
        println!("cargo:rerun-if-changed={p}");
    }

    let mut config = prost_build::Config::new();
    if env::var_os("PROTOC").is_none()
        && let Some(home) = env::var_os("HOME")
    {
        let pinned = PathBuf::from(home).join(".local/protoc-34.1/bin/protoc");
        if pinned.is_file() {
            config.protoc_executable(pinned);
        }
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(out_dir.join("sandbox_descriptor.bin"))
        .compile_with_config(config, PROTOS, &[PROTO_ROOT])?;
    Ok(())
}
