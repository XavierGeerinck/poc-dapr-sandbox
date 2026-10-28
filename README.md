# Dapr Sandbox

A proof of concept for a Dapr-native alpha Sandbox building block: sandbox IDs are internal Dapr actors backed by pluggable microVM drivers (with a Rust microVM driver in this repo). The goal is that a new sandbox runs its first command in p99 < 100 ms on Kubernetes.

- [Design spec](docs/superpowers/specs/2026-10-28-dapr-sandbox-building-block-design.md)
- [Implementation plan](docs/superpowers/plans/2026-10-28-dapr-sandbox-building-block.md)
