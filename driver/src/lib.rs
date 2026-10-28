//! Dapr sandbox driver that hosts microVMs.
//!
//! The driver is a Dapr pluggable component: it serves the
//! `dapr.proto.components.v1.Sandbox` gRPC service on a Unix socket, and
//! daprd discovers it through gRPC reflection. The VM layer sits behind
//! [`backend::VmBackend`]; [`pool::Pool`] keeps a warm pool of VMs per
//! component and [`service::SandboxService`] maps the gRPC contract onto the
//! pools.

pub mod backend;
pub mod config;
pub mod pool;
pub mod service;

/// Generated code for `dapr.proto.components.v1` (Sandbox service and its
/// messages, server and client).
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod proto {
    tonic::include_proto!("dapr.proto.components.v1");

    /// Encoded file descriptor set of the Sandbox service, for reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("sandbox_descriptor");
}
