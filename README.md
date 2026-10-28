# Dapr Sandbox

A proof of concept for a Dapr-native alpha Sandbox building block: sandbox IDs are internal Dapr actors backed by pluggable microVM drivers (with a Rust microVM driver in this repo). The goal is that a new sandbox runs its first command in p99 < 100 ms on Kubernetes.

- [Design spec](docs/superpowers/specs/2026-10-28-dapr-sandbox-building-block-design.md)
- [Implementation plan](docs/superpowers/plans/2026-10-28-dapr-sandbox-building-block.md)

## Toolchain

- Go 1.26.10 via `GOTOOLCHAIN=go1.26.10`. Do not use `auto`: a newer brew Go would be selected and regenerated protos would differ (comment-reformat diffs in 9 `.pb.go` files).
- protoc 34.1 at `~/.local/protoc-34.1/bin`.
- Proto plugins come from `make init-proto` in `upstream/dapr`, run under that same `GOTOOLCHAIN`.

```sh
export PATH=$HOME/.local/protoc-34.1/bin:/opt/homebrew/bin:$HOME/go/bin:$PATH GOTOOLCHAIN=go1.26.10
```
