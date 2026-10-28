//! Generates the `dapr.proto.components.v1` Sandbox service (server and
//! client) and a file descriptor set for gRPC reflection.
//!
//! Needs protoc: `$PROTOC` if set, else `protoc` on PATH. This repo pins
//! protoc 34.1 (see the top-level README), so on a dev machine:
//!
//! ```sh
//! export PROTOC=$HOME/.local/protoc-34.1/bin/protoc
//! ```
//!
//! A container build must install protoc or set `PROTOC`.
//!
//! With feature `microsandbox`, it also records where the `microsandbox`
//! crate's own build script installed the runtime pair (`msb` and
//! `libkrunfw`): `$MSB_HOME/{bin,lib}`, else `~/.microsandbox/{bin,lib}`
//! (see `microsandbox_utils::resolve_home`). This repo's
//! `.cargo/config.toml` points `MSB_HOME` at `driver/target/msb-runtime`.
//! The backend reads it as `DSB_MSB_BUILD_HOME`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const PROTO_ROOT: &str = "proto";
const PROTOS: &[&str] = &[
    "proto/dapr/proto/components/v1/common.proto",
    "proto/dapr/proto/components/v1/sandbox.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=PROTOC");
    println!("cargo:rerun-if-env-changed=MSB_HOME");
    if env::var_os("CARGO_FEATURE_MICROSANDBOX").is_some() {
        let home = env::var_os("MSB_HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".microsandbox")));
        if let Some(home) = home {
            println!("cargo:rustc-env=DSB_MSB_BUILD_HOME={}", home.display());
        }
    }
    for p in PROTOS {
        println!("cargo:rerun-if-changed={p}");
    }

    // prost-build itself uses $PROTOC, else `protoc` on PATH; check first so
    // that a missing protoc fails with an actionable message.
    let protoc = env::var_os("PROTOC").unwrap_or_else(|| "protoc".into());
    if let Err(e) = Command::new(&protoc).arg("--version").output() {
        return Err(format!(
            "cannot run protoc ({}: {e}). Install protoc or set PROTOC, e.g. \
             PROTOC=$HOME/.local/protoc-34.1/bin/protoc",
            PathBuf::from(&protoc).display()
        )
        .into());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(out_dir.join("sandbox_descriptor.bin"))
        .compile_protos(PROTOS, &[PROTO_ROOT])?;
    Ok(())
}
