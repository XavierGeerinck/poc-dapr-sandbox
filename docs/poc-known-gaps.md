# PoC known gaps

Findings from the PoC's reviews that were not fixed. Each is one line, with the files it
concerns. Paths starting with `pkg/`, `cmd/`, `dapr/` or `tests/` are in `upstream/dapr`
(branch `feat/sandbox-building-block`); the others are in this repository. Items that the
final fix wave fixed are not listed.

## Before a dapr/dapr PR

- **DCO and history.** The upstream branch has no `Signed-off-by` lines, and its "review fixes"
  and "review minors" commits should be squashed into the commits they fix. Commit 6874e25ac's
  message wrongly says the processor closes a component whose Init failed; a3284ee93 makes that
  true and says so.
- **Security review** of the ACL change in `pkg/api/grpc/workflow_acl.go` (proposal, "Upstream
  changes in the PoC branch").

## API and error mapping (`pkg/api/universal/sandbox.go`, `pkg/messages/predefined.go`)

- **Body limit.** Every daprd needs `--max-body-size 16Mi` for a capped exec, and a response over
  the limit is reported as 429 `ERR_SANDBOX_HOST_FULL` (gRPC "message too large" is
  ResourceExhausted) instead of an output error.
- **Create with a stuck driver** waits up to the Acquire bound on each of its 5 attempts before
  it answers 429 (`pkg/actors/targets/sandbox/factory.go`, `DefaultAcquireTimeout`).
- **A command cut by a rebalance** answers 500 "interrupted" or 410 `ERR_SANDBOX_LOST`, depending
  on whether the claim was cancelled or the sandbox halted first
  (`pkg/actors/targets/sandbox/sandbox.go`, `commandErr`). Neither is run again.
- **At-least-once edge.** A remote host that ran a command but whose response is lost when its
  connection drops gives the caller's router `Unavailable`, which it retries on the new host
  (`pkg/actors/router`, `IsRetryableRemoteError`).
- **Session close** is not cut short by a release or halt; it waits for the driver's answer
  (`sandbox.go`, `sessionClose`).
- **429 while hosting is suspended** (host app without an actor state store) looks the same as a
  temporary "retry later".
- **Invalid-request details** are only logged at debug level; an `ERR_SANDBOX_LOST` without
  ErrorInfo falls back to the plain message.
- **Code shape:** the unmarshal-and-map block is repeated per method; the tracing key
  `sandbox_id` is a literal and is empty on create; `totalUs` is clamped to at least 1.
- **HTTP layer tests** (`pkg/api/http/sandbox_test.go`): error statuses are tested on exec only;
  not-found, and unsafe IDs on create, delete and session close, are not; the component path
  parameter is still `QueryUnescape`d; the `dapr-app-id` header constant may duplicate an
  existing one.

## Actor target and hosting

- **Acquire on a context that died mid-call** still removes the target through the driver-error
  path (`pkg/actors/targets/sandbox/sandbox.go`, `acquire`).
- **Untested paths:** the removed-during-Acquire guard that hands the VM back, and
  `session.start` NotFound turning into lost (`sandbox_test.go`).
- **Host (`pkg/runtime/sandboxhost/host.go`):** the stop-timeout path can register a type after
  `Remove` skipped its unregister (orphan type); `Remove` always returns nil;
  `TestReconcileLoop` does not assert that live IDs reach the driver; no tests for
  skip-while-acquiring, `Ready` error polling, replacing a component, or a construction error.
- **Processor (`pkg/runtime/processor/sandbox/sandbox.go`):** `Close` drops the driver's Close
  error when `Host.Remove` fails; the `Host.Remove` error path has no test.
- **No component close or remove RPC** in the contract (`dapr/proto/components/v1/sandbox.proto`):
  a component removed from daprd keeps its warm pool in the Rust driver, across driver restarts,
  until its state file is deleted (ruling R16a; `driver/src/persist.rs`).
- **One driver process per daprd.** The driver's `Reconcile` releases every VM of a component
  that the calling daprd does not list, so two daprds that share a driver (one socket folder)
  destroy each other's sandboxes (`driver/src/pool.rs`, `reconcile`).
- **Contract deviations from the spec:** `Exec` is unary, with no streaming output, so the
  benchmark measures time to completion; there is no capacity or stats RPC.

## Built-in `sandbox.process` driver (`pkg/components/sandbox/process/process.go`)

- **Shutdown races:** `wg.Add` after unlocking in `Release` and `Reconcile` can race `Close`'s
  `wg.Wait`; `Reconcile` has no closed check; `fill` destroys a late instance in an untracked
  goroutine, so `Close` is not fully synchronous.
- **Exec details:** an absolute `cwd` is accepted while an escaping relative one is rejected; a
  failed refill is not retried; the exit code is -1 on `ErrWaitDelay`; a timed-out stdin write
  may interleave; `MkdirTemp` runs under the driver lock; background processes started by an exec
  live until release.
- **Command lookup** uses daprd's own `PATH`, not the `PATH` it passes to the command
  (`tests/integration/suite/daprd/sandbox/crossappmtls.go` sets `PATH` for this reason).
- **Tests:** no test for `Release` or `Close` killing an active session; some tests use short
  sleeps; `pluggable_test.go`'s fake server shares fields across goroutines without a mutex.
- **Proto:** `dapr/proto/components/v1/sandbox.proto` has a stray double blank line.

## Integration tests (`tests/integration/suite/daprd/sandbox/`)

- **crossapp:** baselines are taken without settling and the "caller unchanged" check is single
  shot; the "type the pool app does not host" subtest relies on the router's placement retry
  (about 5 s of its 10 s bound).
- **pluggable:** a bounded sleep ignores the test context; a lock comment in the fake driver is
  stale.
- **Sentry suites on a busy host.** The framework probes free ports on 127.0.0.1 from 1025 up,
  while daprd, placement and the scheduler dial Sentry as `localhost`. On a Mac where another
  process listens on `[::]:1025` (mailpit, here), every Sentry-based suite, `crossappmtls`
  included, fails to start (`tests/integration/framework/process/ports/ports.go`).

## Rust driver (`driver/`)

- **Pool:** abandoned inline VMs can push the warm count above `warmPoolSize`; a hung backend call
  leaves a replacement waiting forever (`src/pool.rs`).
- **Contract:** pinned by docs only, with no conformance suite; no deterministic late-spawn test
  and no concurrent-acquire stress test; replacement tests gate on a claim-time counter; the
  failed-template retry test has a wide timing window.
- **Persistence and startup (`src/persist.rs`, `src/service.rs`):** `Init` writes and fsyncs
  under a std `Mutex` on a runtime thread; loading stops at one bad directory entry; a SIGTERM
  during template preparation leaks the template VM until the next start reaps it; restore logs
  its setup at info.
- **Exec:** a timeout of 0 has no backstop; `run`, `lost` and `timed_out` are covered only by
  ignored live tests.
- **Home directory:** protection of `~/.microsandbox` depends on cargo's working directory
  (`build.rs` falls back silently); a relative `$HOME` is not handled; `--selftest` needs `HOME`.
- **Warm-up (`warmupCommand`, off by default):** `warm_up` returns Ok when cancelled, so callers
  must re-check closed; inner and outer warm-up timeouts are equal; no replace-during-warm-up
  test.
- **Network device:** a non-empty `setup` gives the template, and so every restored VM, a NIC
  (proposal, finding 2); the driver could apply the favordynmods remount without one.
- **Code shape:** duplicated helpers (`ms` and percentiles in `main.rs` and the live test,
  `destroy_named`); the session machinery and selftest could move to their own modules.

## Benchmark (`bench/`)

- **Report:** rate rows are keyed by (path, workload, mode, rate), so `path-a-rate20` and
  `sweep-rate20` merge; it reads every `*.csv` in the folder without listing them; late ticks are
  summed across rate files; the gate ignores the non-POOL share (warning only); the SVG lacks a
  per-rate n.
- **Runs:** a 200 response with `timedOut` is not flagged; malformed `uint64` timing values are
  counted process-wide; `sweep` ignores a component override and has no cooldown between rates;
  rates start at 10/s; each rate point is a single sample; no test for stale-sidecar removal.
- **Data:** `session-import` rows have an empty `acquire_path`; path (b) on the Mac is bimodal.

## Self-hosted demo (`demo/run-selfhosted.sh`)

- A stale socket after a crash is not removed before the driver starts.
- The leftover-VM check matches by component-name prefix, so another checkout's demo with the
  same component would match.
- `/tmp/dsb-demo/sockets` is created with the default umask; the port check covers TCP LISTEN
  only; the README's port table is copied by hand; daprd announces itself over mDNS on the LAN.

## kind deployment (`deploy/kind/`)

- **No ownership checks:** on Linux `down.sh` deletes any kind cluster named `sandbox`; on macOS
  it deletes any Lima VM named `sandbox`. A failed `kind delete` inside the VM is ignored.
- **up.sh:** it calls `brew install` without checking that brew exists; the one-shot reboot
  marker is written before the VM stops; after patching the kubelet config it does not restart
  pool pods that are already running.
- **Images:** base images are pinned by tag, not digest, and the VM's Docker install is not
  pinned (`lima.yaml`, `build-images.sh`).
- **Manifests:** the `/dev/kvm` group mapping (to `uuidd` in the node image) is only described in
  the Task 15 report; the `/dev/kvm` hostPath is redundant with the privileged container
  (`manifests/sandbox-pool.yaml`).
- **OOM:** the `singleProcessOOMKill` behaviour was configured and checked in the kubelet config,
  but an OOM kill was not exercised live.
