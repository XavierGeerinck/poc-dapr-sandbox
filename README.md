# Dapr Sandbox

A proof of concept for a Dapr-native alpha Sandbox building block: sandbox IDs are internal Dapr actors backed by pluggable microVM drivers (with a Rust microVM driver in this repo). The goal is that a new sandbox runs its first command in p99 < 100 ms on Kubernetes.

Result: on kind on a bare-metal Linux host with KVM, a new sandbox ID served from the warm pool ran its first command at p99 16.44 ms sequentially and p99 11.36 ms at 20 new sandboxes per second per host pod, measured at the caller's sidecar ([`results/kind-linux/`](results/kind-linux/summary.md)). Sequentially, 18 of the 500 requests found the pool empty and needed a restore; over all 500 rows the p99 was 103.9 ms. The results were measured at upstream b0ce41dac, before the final fix wave (error paths only, not re-measured).

- [Design spec](docs/superpowers/specs/2026-10-28-dapr-sandbox-building-block-design.md)
- [Implementation plan](docs/superpowers/plans/2026-10-28-dapr-sandbox-building-block.md)
- Drafts for review, not posted: [proposal](proposal/20261028-BRCS-sandbox-building-block.md), [dapr/dapr issue](proposal/issue.md), [PR description](proposal/pr.md), [microsandbox issue](proposal/microsandbox-issue.md)
- [Known gaps](docs/poc-known-gaps.md): what the PoC's reviews found and did not fix

## What is where

| Path | What |
|---|---|
| `driver/` | The microVM driver, a Dapr pluggable component (Rust, microsandbox 0.7.11) |
| `bench/` | The latency benchmark and its gate report |
| `demo/` | The self-hosted demo on macOS ([`demo/README.md`](demo/README.md)) |
| `deploy/kind/` | kind on macOS (through Lima) or on Linux with KVM, and its manifests |
| `proposal/` | The drafts above, and the Dapr changes as patches (`dapr-poc-patches/`) |
| `results/` | Measured results (see [Results](#results)) |
| `upstream/dapr` | A `dapr/dapr` checkout on branch `feat/sandbox-building-block` (gitignored) |

## The Dapr branch

The demo and the kind images build Dapr from `upstream/dapr`. It is `dapr/dapr` at `v1.19.0` plus the patches in `proposal/dapr-poc-patches/`:

```sh
git clone https://github.com/dapr/dapr upstream/dapr
git -C upstream/dapr checkout -b feat/sandbox-building-block v1.19.0
git -C upstream/dapr am "$PWD"/proposal/dapr-poc-patches/*.patch
```

After changing the branch, `scripts/export-patches.sh` writes the patches again.

## Quickstart: self-hosted (macOS, Apple silicon)

One daprd and one driver on the Mac, with microVMs on Hypervisor.framework. Needs Go (the version in `upstream/dapr/go.mod`), Rust 1.94 or later and protoc; see [`demo/README.md`](demo/README.md).

```sh
demo/run-selfhosted.sh up     # build, start, wait until the warm pool is ready
demo/smoke.sh                 # exec, Python session, stale activation, delete
demo/run-selfhosted.sh down   # stop everything; prints "no leftover VMs"
```

## Quickstart: Kubernetes (kind)

`deploy/kind/up.sh` picks the mode from the host:
- **macOS (Apple silicon, M3 or later):** a Lima VM with nested virtualization runs Docker and kind. It works, but the latency is not representative: every guest exit traps through two hypervisors (see `results/kind/`). `up.sh` installs Lima and helm with Homebrew if they are missing; `kubectl` must be installed; building Dapr (`build-images.sh`) needs Go and `make`.
- **Linux with `/dev/kvm`:** Docker and kind run directly on the host. Needs `docker`, `kind`, `kubectl`, `helm`, `make` and `git`; nothing is installed for you.

The scripts never touch `~/.kube/config`. The cluster's kubeconfig is `<repo>/.kube/kind-sandbox.yaml`, context `kind-sandbox`:

```sh
export KUBECONFIG=$PWD/.kube/kind-sandbox.yaml
deploy/kind/up.sh                     # the kind cluster "sandbox" (and on macOS the Lima VM)
deploy/kind/build-images.sh           # Dapr from upstream/dapr, the driver and the bench, loaded into kind
helm install dapr upstream/dapr/charts/dapr -n dapr-system --create-namespace \
  --kube-context kind-sandbox -f deploy/kind/helm-values.yaml --wait
kubectl --context kind-sandbox apply -f deploy/kind/manifests/
deploy/kind/smoke.sh                  # 10 fresh sandboxes from the caller pod, spread over both pool pods
deploy/kind/down.sh                   # delete the cluster and its kubeconfig (and on macOS the VM)
```

What the manifests deploy:
- `sandbox-pool`: the sandbox host, 2 pods.
- `sandbox-caller`: a separate app that reaches the sandboxes through its own daprd with the header `dapr-app-id: sandbox-pool`. The benchmark runs in it (see `results/kind-linux/notes.md` for the commands).

Plainly, about privileges and kubelet settings:
- **The driver container (`microvm`) is privileged.** It needs that to open `/dev/kvm`. daprd and the caller are not privileged.
- **The kubelets set `singleProcessOOMKill: true`** (`deploy/kind/kind.yaml`). Without it, one OOM kill takes the driver and every VM in the pod.

## daprd needs `--max-body-size 16Mi`

Every daprd that hosts or calls sandboxes runs with `--max-body-size 16Mi`. On Kubernetes, that is the annotation `dapr.io/max-body-size: 16Mi`.

- **Why.** The driver caps each exec output stream at 4194304 bytes, and a response carries two (stdout and stderr). daprd's default limit, which also applies to the pluggable component connection, is too small for a capped exec.
- **Over the limit.** A client gets 429 instead of `truncated: true`.
- **Where it is set.** `demo/run-selfhosted.sh` and the kind manifests set it already.

## Drivers: what to know

- **One driver process per daprd.** daprd's periodic `Reconcile` tells the driver which VMs it still owns, and the driver releases the others. Two daprds that share a driver (the same components socket folder) therefore destroy each other's sandboxes.
- **A removed component keeps its warm pool.** The contract has no component close or remove call. When a component is removed from daprd, the microVM driver keeps its warm pool, also across driver restarts, until the component's state file is deleted (`<state dir>/<sha256(component)>.json`, see `driver/src/persist.rs`). On kind the state directory is a pod-scoped `emptyDir`, and the demo's `down` deletes its own.
- **`sandbox.process` has no isolation.** daprd's built-in driver runs commands as host processes. It is for tests: it is not in stable builds (only with the `allcomponents` or `sandbox_process` build tag), and its Init fails unless the component sets `allowNoIsolation: "true"`.

## Results

| Folder | What | Gate |
|---|---|---|
| [`results/kind-linux/`](results/kind-linux/notes.md) | kind on a bare-metal Linux host with KVM (AMD Ryzen 9 5900X): the Kubernetes gate run | PASS |
| [`results/kind/`](results/kind/notes.md) | kind on macOS in a Lima VM, under nested virtualization; also the live previews and the warm-up experiment from Task 16 | FAIL (nested stack) |
| [`results/mac-favordynmods/`](results/mac-favordynmods/notes.md) | self-hosted on the Mac (Apple M4 Pro), with the favordynmods `setup` | PASS |
| [`results/mac/`](results/mac/notes.md) | self-hosted on the Mac, before favordynmods; also the driver selftest, the stretch goal and the warm-up experiment | PASS |
| [`results/idle-cliff/`](results/idle-cliff/notes.md) | root cause of an exec delay after idle in microsandbox's guest, and its fix | (investigation) |

Each benchmark folder holds:
- `summary.md`, the benchmark's report with the gate line;
- `notes.md`, with the setup, the commands and per-run breakdowns;
- `raw/`, the CSVs;
- `rate-sweep.svg`.

`results/idle-cliff/` holds notes only, and the Task 16 live previews and warm-up experiment in `results/kind/notes.md` have no raw CSVs: their values were read from the API responses at the time.

## Windows

Run it inside WSL2 with nested virtualization, which provides `/dev/kvm`, and use the Linux mode of `deploy/kind/`. This PoC did not test WSL2.
- **Why not native Windows.** Dapr pluggable components talk gRPC over Unix domain sockets, and Dapr's own tests skip them on Windows.
- **Future work.** Native Windows (WHP) support.

## Toolchain

- Go 1.26.10 via `GOTOOLCHAIN=go1.26.10`. Do not use `auto`: a newer brew Go would be selected and regenerated protos would differ (comment-reformat diffs in 9 `.pb.go` files).
- protoc 34.1 at `~/.local/protoc-34.1/bin`.
- Proto plugins come from `make init-proto` in `upstream/dapr`, run under that same `GOTOOLCHAIN`.

```sh
export PATH=$HOME/.local/protoc-34.1/bin:/opt/homebrew/bin:$HOME/go/bin:$PATH GOTOOLCHAIN=go1.26.10
```
