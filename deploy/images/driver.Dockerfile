# The microVM sandbox driver, dapr-sandbox-microvm, with its microsandbox
# runtime pair (msb, libkrunfw). Built for the build host's architecture
# from the repo root, inside the Lima VM that deploy/kind/up.sh starts
# (on a Linux host, directly with docker):
#
#   limactl shell --workdir <repo> sandbox \
#     docker build -f deploy/images/driver.Dockerfile -t dapr-sandbox-microvm:dev .
#   limactl shell sandbox kind load docker-image dapr-sandbox-microvm:dev --name sandbox
#
# The build context is only driver/ (driver.Dockerfile.dockerignore).
# Running it needs /dev/kvm and a privileged container (deploy/kind/kvm-check.yaml).

ARG RUST_VERSION=1.94.0
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN} AS build
# driver/build.rs compiles the Sandbox protos with protoc (proto3 only, which
# Debian's protoc 3.21 handles).
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
# The `microsandbox` crate's build script (feature `download-binaries`)
# downloads the runtime pair for the target (the build host's architecture) into
# $MSB_HOME/{bin,lib}, and driver/build.rs bakes that directory into the
# binary (DSB_MSB_BUILD_HOME), where the driver looks for the pair at run
# time. The repo's .cargo/config.toml (which forces MSB_HOME to
# driver/target/msb-runtime) is not in the build context, so this applies,
# and the runtime stage copies the pair to the same path.
ENV MSB_HOME=/opt/dsb/msb-runtime
WORKDIR /src/driver
COPY driver/ ./
RUN --mount=type=cache,id=dsb-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=dsb-driver-target,target=/src/driver/target \
    cargo build --release --locked --bin dapr-sandbox-microvm \
 && install -D target/release/dapr-sandbox-microvm /out/dapr-sandbox-microvm \
 && test -x "$MSB_HOME/bin/msb" && ls "$MSB_HOME"/lib/libkrunfw.so.*

FROM debian:${DEBIAN}-slim
# CA roots: microsandbox pulls OCI images over TLS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /opt/dsb/msb-runtime /opt/dsb/msb-runtime
COPY --from=build /out/dapr-sandbox-microvm /usr/local/bin/dapr-sandbox-microvm
# The microsandbox home holds each VM's control socket at <home> plus 52
# bytes, which must fit a 108-byte socket path: keep it short. Pods mount a
# volume (emptyDir) at /var/lib/dsb.
ENV DAPR_SANDBOX_MSB_HOME=/var/lib/dsb/msb \
    DAPR_SANDBOX_STATE_DIR=/var/lib/dsb/state
ENTRYPOINT ["dapr-sandbox-microvm"]
