# The caller pod's image (deploy/kind/manifests/caller.yaml): the latency
# harness sandbox-bench (bench/), plus curl and jq for deploy/kind/smoke.sh.
# Built for the build host's architecture (linux/arm64), from the repo root,
# inside the Lima VM that deploy/kind/up.sh starts; deploy/kind/build-images.sh
# builds it and loads it into the kind nodes:
#
#   limactl shell --workdir <repo> sandbox \
#     docker build -f deploy/images/bench.Dockerfile -t sandbox-bench:dev .
#
# The build context is only bench/ (bench.Dockerfile.dockerignore). The bench
# speaks plain HTTP to its own daprd (reqwest without TLS), so it needs no
# protoc, OpenSSL or CA roots.

ARG RUST_VERSION=1.94.0
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN} AS build
WORKDIR /src/bench
COPY bench/ ./
RUN --mount=type=cache,id=dsb-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=dsb-bench-target,target=/src/bench/target \
    cargo build --release --locked --bin sandbox-bench \
 && install -D target/release/sandbox-bench /out/sandbox-bench

FROM debian:${DEBIAN}-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl jq \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/sandbox-bench /usr/local/bin/sandbox-bench
# An unprivileged user (the same uid as daprd's image; no passwd entry is
# needed). The caller pod mounts an emptyDir at /work for the bench's CSVs.
USER 65532:65532
WORKDIR /work
ENTRYPOINT ["sandbox-bench"]
