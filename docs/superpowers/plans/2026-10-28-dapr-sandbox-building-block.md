# Dapr Sandbox Building Block Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an alpha Sandbox building block to `daprd`, where a sandbox ID is an internal Dapr actor backed by a pluggable microVM driver. Prove a new sandbox ID runs its first command in p99 < 100 ms on Kubernetes (kind), cross-pod.

**Architecture:** `daprd` (a branch of dapr/dapr v1.19.0) gets:
- a `sandbox` component category whose drivers are gRPC pluggable components;
- one internal actor type per sandbox component (`dapr.internal.<ns>.<appID>.sandbox.<component>`);
- an alpha HTTP/gRPC API that turns requests into actor calls.

A Rust pluggable component in this repo drives microVMs through microsandbox. It keeps a warm pool restored from a template snapshot. A benchmark measures latency from the caller sidecar, first on the Mac (self-hosted), then on kind inside a Lima VM with nested virtualization.

**Tech Stack:** Go 1.26.10 (Dapr), protoc 34.1, Rust (tokio, tonic, prost), microsandbox 0.7.11, Dapr CLI self-hosted, Lima (vz, nested virtualization), kind, Helm.

**Spec:** `docs/superpowers/specs/2026-10-28-dapr-sandbox-building-block-design.md`

## Global Constraints

- Base: dapr/dapr tag `v1.19.0`, cloned with `--filter=blob:none` to `upstream/dapr` (gitignored), branch `feat/sandbox-building-block`.
- Go `1.26.10` (from `go.mod`). protoc `34.1`, protoc-gen-go `v1.32.0`, protoc-gen-go-grpc `1.3.0`, protoc-gen-connect-go `1.18.1` (`Makefile:49,94-99`).
- `daprd` builds with `CGO_ENABLED=0 -tags=allcomponents`. No VMM code is linked into `daprd`.
- microsandbox: `microsandbox = { version = "=0.7.11", default-features = false, features = ["local", "download-binaries", "net"] }`.
- **Never push, open PRs or issues, or post anywhere.** Never use kube contexts `aks-scrydon-ops`, `aks-scrydon-prod` or `klina-aks`.
  - Every kubectl/helm command uses `KUBECONFIG=$REPO/.kube/kind-sandbox.yaml` and `--context kind-sandbox` (`--kube-context` for helm).
- API version `v1.0-alpha1`. Actor type `dapr.internal.<namespace>.<hostAppID>.sandbox.<component>`.
- Component metadata names and defaults:

  | Name | Default |
  |---|---|
  | `image` | required |
  | `cpus` | `1` |
  | `memoryMiB` | `512` |
  | `setup` | empty |
  | `warmPoolSize` | `4` |
  | `maxSandboxesPerHost` | `32` |
  | `idleTimeout` | `5m` |
  | `template` | `snapshot` (or `none`) |
  | `refillConcurrency` | `2` |

- Target app header: `dapr-app-id` (overrides the body's `appId`; defaults to the caller's app ID).
- Error codes:

  | Code | HTTP | gRPC |
  |---|---|---|
  | `ERR_SANDBOX_NOT_FOUND` | same status as `ErrBinaryStoreNotFound` | same code as `ErrBinaryStoreNotFound` |
  | `ERR_SANDBOX_HOST_FULL` | 429 | `ResourceExhausted` |
  | `ERR_SANDBOX_LOST` | 410 | `FailedPrecondition` |
  | `ERR_SANDBOX_INVALID_REQUEST` | 400 | `InvalidArgument` |
  | `ERR_SANDBOX_EXEC` | 500 | `Internal` |

- **Latency metric:** from the moment the exec request reaches the caller's sidecar to the first byte of output from a trivial command. The gate value is `timing.totalUs`, measured by the caller's `daprd` around the whole call.
- **Gate:**
  - **What:** path (a), a new ID served from the warm pool (`acquirePath == POOL`).
  - **Threshold:** p99 `totalUs` < 100 000 µs.
  - **Where:** on kind, cross-pod.
  - **Load:** sequential, and a sustained 20 new sandboxes/s per host pod.
- Exec output cap: 4 MiB per stream. Beyond it, truncate and set `truncated=true`.
- **Stop and report to the user (don't work around):**
  - path (a) misses because of the microsandbox exec relay;
  - the Task 15 spike fails;
  - a clear path (a) miss on the Mac (Task 14).
- Commits: this repo on the current branch; dapr changes on `feat/sandbox-building-block` in `upstream/dapr`. Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Two callers hit the same brand-new sandbox ID at once.** Expected: exactly one driver `Acquire`, and both calls run in that one VM. Test in Task 5.
2. **The driver process crashes or restarts while `daprd` still holds instance IDs.** Expected: the next call returns `ERR_SANDBOX_LOST` quickly (no hang) and the actor deactivates. Tests in Tasks 5 and 10.
3. **Sandbox IDs that aren't safe VM names** (`:`, `/`-encoded, unicode, 300 bytes). Expected: they work end to end, because VM names are always driver-generated and never derived from the ID. Tests in Tasks 8 and 10.
4. **Runaway commands** (infinite loop, 10 MiB of output). Expected: `timeoutMs` kills them with a non-zero exit; output is capped at 4 MiB with `truncated=true`; the sandbox stays usable. Tests in Tasks 4 and 11.
5. **Component removed or `daprd`/driver shut down with live sandboxes.** Expected: every VM is released and no `msb` processes are left behind. Tests in Tasks 6 and 10.

---

## Repository layout

```
.gitignore                     upstream/, target/, .kube/, results/*/raw/*.csv.tmp
upstream/dapr/                 dapr/dapr clone (gitignored); branch feat/sandbox-building-block
driver/                        Rust crate dapr-sandbox-microvm (pluggable component)
  proto/dapr/proto/components/v1/{common,sandbox}.proto   synced copy
  src/{main,service,pool,config,backend,microsandbox_backend}.rs
bench/                         Rust crate sandbox-bench
demo/                          self-hosted components + scripts
deploy/kind/                   lima.yaml, kind.yaml, k8s manifests, helm values
deploy/images/driver.Dockerfile
scripts/{sync-proto,check-proto-sync,export-patches}.sh
proposal/                      proposal, issue.md, pr.md, dapr-poc-patches/
results/{mac,kind}/            raw CSV, summary.md, rate-sweep.svg
```

---

## Part 0: Setup

### Task 1: Workspace and toolchains

**Files:**
- Create: `.gitignore`, `README.md` (one paragraph plus links to spec and plan), `scripts/export-patches.sh`

**Interfaces:**
- Produces: `upstream/dapr` on branch `feat/sandbox-building-block`, `go` and `protoc 34.1` on PATH, and `scripts/export-patches.sh`, which runs `git -C upstream/dapr format-patch v1.19.0..feat/sandbox-building-block -o proposal/dapr-poc-patches/`.

- [ ] **Step 1:** `brew install go` (any recent Go; `GOTOOLCHAIN=auto` fetches 1.26.10 from `go.mod`). Install protoc 34.1 from the protobuf GitHub release (`protoc-34.1-osx-aarch_64.zip`) into `~/.local/protoc-34.1` and put it on PATH.
- [ ] **Step 2:** `git clone --filter=blob:none --branch v1.19.0 https://github.com/dapr/dapr upstream/dapr && git -C upstream/dapr switch -c feat/sandbox-building-block`. Then `cd upstream/dapr && make init-proto`.
- [ ] **Step 3: Verify the baseline builds and tests pass.**
  - `cd upstream/dapr && go version` → `go1.26.10`.
  - `make build BINARIES=daprd` → `dist/darwin_arm64/release/daprd` exists.
  - `go test -tags="allcomponents unit" ./pkg/api/http -run TestBinaryStore` → PASS.
  - `protoc --version` → `libprotoc 34.1`.
- [ ] **Step 4:** `make gen-proto-components gen-proto-runtime` and then `git -C upstream/dapr status --short` → empty. This proves the toolchain reproduces the checked-in generated code byte for byte.
- [ ] **Step 5: Commit** `.gitignore README.md scripts/export-patches.sh` with the message `chore: workspace scaffold for Dapr sandbox PoC`.

---

## Part A: Dapr runtime (all paths relative to `upstream/dapr`)

### Task 2: Component contract and Go driver interface

**Files:**
- Create: `dapr/proto/components/v1/sandbox.proto`
- Create: `pkg/components/sandbox/sandbox.go` (interface), `config.go`, `registry.go`, `pluggable.go`
- Test: `pkg/components/sandbox/config_test.go`, `pluggable_test.go`
- Generated: `pkg/proto/components/v1/sandbox*.go`

**Interfaces:**
- Produces: this proto (package `dapr.proto.components.v1`, `go_package` like `secretstore.proto:20`):

```proto
service Sandbox {
  rpc Init(SandboxInitRequest) returns (SandboxInitResponse) {}
  rpc Features(FeaturesRequest) returns (FeaturesResponse) {}
  rpc Ping(PingRequest) returns (PingResponse) {}
  rpc Ready(SandboxReadyRequest) returns (SandboxReadyResponse) {}
  rpc Acquire(SandboxAcquireRequest) returns (SandboxAcquireResponse) {}   // RESOURCE_EXHAUSTED when full
  rpc Exec(SandboxExecRequest) returns (SandboxExecResponse) {}            // NOT_FOUND if instance unknown
  rpc SessionStart(SandboxSessionStartRequest) returns (SandboxSessionStartResponse) {}
  rpc SessionSend(SandboxSessionSendRequest) returns (SandboxExecResponse) {}
  rpc SessionClose(SandboxSessionCloseRequest) returns (SandboxSessionCloseResponse) {}
  rpc Release(SandboxReleaseRequest) returns (SandboxReleaseResponse) {}   // returns immediately
  rpc Reconcile(SandboxReconcileRequest) returns (SandboxReconcileResponse) {}
}
message SandboxInitRequest { MetadataRequest metadata = 1; }
message SandboxInitResponse {}
message SandboxReadyRequest {}
message SandboxReadyResponse { bool ready = 1; string reason = 2; }
enum SandboxAcquirePath { SANDBOX_ACQUIRE_PATH_UNSPECIFIED = 0; POOL = 1; RESTORE = 2; BOOT = 3; }
message SandboxAcquireRequest { string sandbox_id = 1; string activation_id = 2; }
message SandboxAcquireResponse { string instance_id = 1; SandboxAcquirePath path = 2; uint64 elapsed_us = 3; }
message SandboxExecSpec { string command = 1; repeated string args = 2; map<string,string> env = 3; string cwd = 4; bytes stdin = 5; uint32 timeout_ms = 6; }
message SandboxExecRequest { string instance_id = 1; SandboxExecSpec spec = 2; }
message SandboxExecResponse { bytes stdout = 1; bytes stderr = 2; int32 exit_code = 3; bool truncated = 4; bool timed_out = 5; uint64 elapsed_us = 6; }
message SandboxSessionStartRequest { string instance_id = 1; SandboxExecSpec spec = 2; }
message SandboxSessionStartResponse { string session_id = 1; }
message SandboxSessionSendRequest { string instance_id = 1; string session_id = 2; bytes input = 3; bytes until = 4; uint32 timeout_ms = 5; }
message SandboxSessionCloseRequest { string instance_id = 1; string session_id = 2; }
message SandboxSessionCloseResponse {}
message SandboxReleaseRequest { string instance_id = 1; }
message SandboxReleaseResponse {}
message SandboxReconcileRequest { repeated string live_instance_ids = 1; }
message SandboxReconcileResponse { uint32 released = 1; }
```

- Produces (Go, package `sandbox`):
  - `type Sandbox interface`, with one method per RPC except Features/Ping. Each takes `(ctx, *proto.XRequest)` and returns `(*proto.XResponse, error)`, except `Init(ctx context.Context, md metadata.Base) error` and `Close() error`.
  - `type Config struct{ Image string; CPUs uint32; MemoryMiB uint32; Setup string; WarmPoolSize, MaxSandboxesPerHost, RefillConcurrency uint32; IdleTimeout time.Duration; Template string }`
  - `func ParseConfig(props map[string]string) (Config, error)`
  - `Registry` mirroring `pkg/components/binarystore/registry.go` (prefix `sandbox.`): `RegisterComponent(f func(logger.Logger) Sandbox, names ...string)`, `Create(name, version, logName string) (Sandbox, error)`, `DefaultRegistry`
  - `func NewGRPCSandbox(l logger.Logger, socket string) Sandbox`
  - an `init()` calling `pluggable.AddServiceDiscoveryCallback("dapr.proto.components.v1.Sandbox", …)` that registers into `DefaultRegistry`, following `pkg/components/secretstores/pluggable.go:105-129`

- [ ] **Step 1: Write failing tests.**
  - `TestParseConfig`:
    - `{"image":"x"}` → defaults exactly as Global Constraints;
    - missing `image` → error;
    - `idleTimeout:"90s"` → 90 s;
    - `template:"bogus"` → error.
  - `TestGRPCSandbox_RoundTrip`:
    - start an in-process `proto.SandboxServer` fake on a Unix socket in `t.TempDir()`;
    - `NewGRPCSandbox(...).Init` must send the metadata properties, and the fake must see header `x-component-instance` = component name;
    - `Acquire` returns the fake's `instance_id`;
    - a fake `codes.ResourceExhausted` comes back with `status.Code(err) == codes.ResourceExhausted`.
- [ ] **Step 2: Run them and confirm they fail.** `go test -tags="allcomponents unit" ./pkg/components/sandbox/...` → FAIL (undefined).
- [ ] **Step 3:** Write the proto, then run `make gen-proto-components`. Implement the files above.
- [ ] **Step 4: Run the tests and confirm they pass.** Same command → PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): component contract and pluggable client`.

### Task 3: Category wiring

**Files (modify; line refs are v1.19.0):**
- `pkg/components/category.go:19-35` — add `CategorySandbox Category = "sandbox"`.
- `pkg/runtime/compstore/sandbox.go` (create) + `compstore.go:80,110`
  - Produces: `AddSandbox(name string, s sandbox.Sandbox)`, `GetSandbox(name string) (sandbox.Sandbox, bool)`, `DeleteSandbox(name string)`.
- `pkg/runtime/processor/sandbox/sandbox.go` (create)
  - Produces: `Options{Registry *sandbox.Registry; Store *compstore.ComponentStore; Meta *meta.Meta; Host *sandboxhost.Host}`, `New(Options) *sandbox`, plus `Init(ctx, compapi.Component) error` and `Close(compapi.Component) error`.
  - Pattern: `processor/binarystore/binarystore.go:43-93`.
  - `Init`: create → `ParseConfig` → driver `Init` → `AddSandbox` → `Host.Add`.
  - `Close`: `Host.Remove` → `DeleteSandbox` → driver `Close`.
- `pkg/runtime/processor/processor.go` — wire at the same points as binarystore (:52, :163, :271-275, :305, :385-391, :406, :468). The Host comes from `sandboxhost.New(sandboxhost.Options{Actors: opts.Actors, AppID: opts.ID, Namespace: opts.Namespace})`.
- `pkg/runtime/processor/manager.go:96-112`.
- `pkg/runtime/registry/registry.go:61,80,134` and `options.go:46,65,142` — `WithSandboxes`.
- `cmd/daprd/app/app.go:30,108,170`.
- `pkg/messages/errorcodes/errorcodes.go` and `pkg/messages/predefined.go` — the five error codes (Global Constraints).
- `pkg/resiliency/resiliency.go:69`, `pkg/diagnostics/consts/consts.go:57`.
- Test: `pkg/runtime/processor/processor_test.go:226`, `pkg/runtime/processor/sandbox/sandbox_test.go`.

**Interfaces:**
- Consumes: Task 2 `Sandbox`, `Registry`, `ParseConfig`.
- Consumes: Task 6 `sandboxhost.Host`. To build this task first, define it in Task 6's package as a stub with the final signatures; Task 6 fills it in.

- [ ] **Step 1: Write failing tests.**
  - `TestExtractComponentCategory`: add case `"sandbox.microvm"` → `CategorySandbox`.
  - `TestSandboxProcessor_InitClose`:
    - with a fake driver registered as `sandbox.fake`, `Init` of a component with `image: x` adds it to the compstore and calls `Host.Add` once with the parsed `Config`;
    - `Close` removes it and calls driver `Close`;
    - a component with no `image` → `Init` returns an error and adds nothing.
- [ ] **Step 2: Confirm they fail:** `go test -tags="allcomponents unit" ./pkg/runtime/processor/...` → FAIL.
- [ ] **Step 3:** Implement the wiring above.
- [ ] **Step 4: Confirm they pass:** the same command, plus `make build BINARIES=daprd` → both PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): register sandbox component category`.

### Task 4: Built-in `sandbox.process` driver

**Files:**
- Create: `pkg/components/sandbox/process/process.go`, `cmd/daprd/components/sandbox_process.go` (register `process.New` as `"process"` in `DefaultRegistry`, same build-tag pattern as the neighbouring files)
- Test: `pkg/components/sandbox/process/process_test.go`

**Interfaces:**
- Produces: `func New(l logger.Logger) sandbox.Sandbox`.
- **Behaviour:**
  - Each instance is a temp dir; commands run as host processes with `cwd` defaulting to that dir.
  - The `warmPoolSize` dirs are pre-created: `Acquire` takes one, reports path `POOL`, and refills in the background. With an empty pool it creates one inline and reports `BOOT`.
  - `maxSandboxesPerHost` is enforced.
  - Sessions use `os/exec` with a stdin pipe; `SessionSend` reads stdout+stderr until `until` appears or the timeout passes.
  - **No isolation**, and the package doc says so.

- [ ] **Step 1: Write failing tests.**
  - `TestAcquirePoolThenBoot`: `warmPoolSize: 1` → the first `Acquire` returns `POOL`, an immediate second returns `BOOT`.
  - `TestHostFull`: `maxSandboxesPerHost: 1` → the second `Acquire` returns `codes.ResourceExhausted`.
  - `TestExecEcho`: `echo hi` → stdout `hi\n`, exit 0.
  - `TestExecTimeout`: `sleep 5` with `timeout_ms: 200` → `timed_out=true`, non-zero exit, elapsed < 1 s. A following `echo ok` on the same instance → `ok\n`.
  - `TestExecOutputCap`: `head -c 10485760 /dev/zero` → stdout length 4194304, `truncated=true`.
  - `TestSessionKeepsState`: `sh -i`, send `X=42\n`, then `echo $X\n` with `until` `42` → stdout contains `42`.
  - `TestExecUnknownInstance`: → `codes.NotFound`.
  - `TestReconcile`: three acquired, live = [first] → released 2.
- [ ] **Step 2: Confirm they fail.**
- [ ] **Step 3:** Implement.
- [ ] **Step 4: Confirm they pass:** `go test -tags="allcomponents unit" ./pkg/components/sandbox/process/` → PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): built-in process driver for tests`.

### Task 5: Sandbox actor target

**Files:**
- Create: `dapr/proto/runtime/v1/sandbox.proto` (messages only; split-file header like `binarystore.proto:16-24`). Generated with `make gen-proto-runtime`.
- Create: `pkg/actors/targets/sandbox/factory.go`, `sandbox.go`
- Test: `pkg/actors/targets/sandbox/sandbox_test.go`

**Interfaces:**
- Consumes: Task 2 `sandbox.Sandbox`.
- Produces these runtime messages (`camelCase` JSON via protojson). Task 7 adds the rpcs that use them.
  - `SandboxTimingAlpha1{totalUs, hostUs, activateUs, execUs uint64; acquirePath string}` — `acquirePath` is the driver enum's name: `POOL`, `RESTORE` or `BOOT`
  - `SandboxExecRequestAlpha1{component, sandboxId, appId, expectedActivation, command string; args []string; env map; cwd string; stdin bytes; timeoutMs uint32}`
  - `SandboxExecResponseAlpha1{sandboxId, activation string; stdout, stderr bytes; exitCode int32; truncated, timedOut bool; timing SandboxTimingAlpha1}`
  - `SandboxCreateRequestAlpha1{component, appId}`
  - `SandboxCreateResponseAlpha1{sandboxId, activation; timing}`
  - `SandboxDeleteRequestAlpha1{component, sandboxId, appId}`
  - `SandboxSessionStartRequestAlpha1{component, sandboxId, appId, expectedActivation, command, args, env, cwd}`
  - `SandboxSessionStartResponseAlpha1{sessionId, activation}`
  - `SandboxSessionSendRequestAlpha1{component, sandboxId, appId, expectedActivation, sessionId string; input, until bytes; timeoutMs uint32}` → returns `SandboxExecResponseAlpha1`
  - `SandboxSessionCloseRequestAlpha1{component, sandboxId, appId, sessionId}`
- Produces (actor target):
  - `type Options struct{ ActorType, Component string; Driver sandbox.Sandbox; IdleTimeout time.Duration; Clock clock.Clock }`
  - `func New(opts Options) *Factory`. It implements `targets.Factory` (`pkg/actors/targets/targets.go:34-41`), plus `func (f *Factory) LiveInstanceIDs() []string`.
  - Method names: `MethodActivate = "activate"`, `MethodExec = "exec"`, `MethodRelease = "release"`, `MethodSessionStart = "session.start"`, `MethodSessionSend = "session.send"`, `MethodSessionClose = "session.close"`.
  - `InvokeMethod` request data is the proto-marshalled runtime request.
  - The response data is the proto-marshalled runtime response, with `activation` set and `timing.hostUs/activateUs/execUs/acquirePath` filled in.
- **Behaviour:**
  - **Activation token:** a new UUID when a target is created (`GetOrCreate`).
  - **Acquiring the VM:** the driver `Acquire` happens lazily on the first `InvokeMethod`, under a per-target mutex that every call holds (turn-based).
  - **Lost-sandbox check:** if the request has `expectedActivation` and it doesn't equal the token → return `FailedPrecondition` (ErrorInfo reason `DAPR_SANDBOX_LOST`) **before** acquiring, and remove the target.
  - **Host full:** driver `ResourceExhausted` → return it and remove the target.
  - **Instance gone:** driver `NotFound` on exec → `FailedPrecondition`/`DAPR_SANDBOX_LOST`, and remove the target.
  - **Releasing:** `Deactivate`, idle expiry and `release` remove the target first, then call driver `Release` in a goroutine with a 10 s timeout. They never block the factory lock on the driver.
  - **Idle expiry:** uses `dapr/kit` `events/queue` like `pkg/actors/targets/app/factory.go:84-95`, reset at the start of each call.

- [ ] **Step 1: Write failing tests** (fake driver that counts calls and can delay or fail).
  - `TestFirstCallAcquiresOnce`: 2 concurrent `exec` calls on a new ID → `Acquire` count 1, both responses have the same `activation`.
  - `TestExpectedActivationMismatch`: a stale token → `FailedPrecondition`, `Acquire` count 0.
  - `TestHostFullRemovesTarget`: → `ResourceExhausted`, and `Exists(id)` is false.
  - `TestDriverNotFoundIsLost`: → `FailedPrecondition`.
  - `TestDeactivateDoesNotBlock`: driver `Release` sleeps 5 s → `Deactivate` returns in < 50 ms.
  - `TestIdleExpiry`: with a fake clock, 5 m idle → `Release` called once, and `LiveInstanceIDs()` is empty.
  - `TestTimingFilled`: `activateUs>0`, `execUs>0`, `acquirePath==POOL` from the fake.
- [ ] **Step 2: Confirm they fail:** `go test -tags="allcomponents unit" ./pkg/actors/targets/sandbox/` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4: Confirm they pass** (also run with `-race`).
- [ ] **Step 5: Commit** with the message `feat(sandbox): internal actor target backed by sandbox drivers`.

### Task 6: Sandbox host (registration, readiness, reconcile)

**Files:**
- Create: `pkg/runtime/sandboxhost/host.go`
- Test: `pkg/runtime/sandboxhost/host_test.go`

**Interfaces:**
- Consumes: `actors.Interface` (`Table(ctx)`, `pkg/actors/actors.go:101-119`), `table.RegisterActorTypes/UnRegisterActorTypes`, Task 5 `New`/`LiveInstanceIDs`.
- Produces:
  - `type Options struct{ Actors actors.Interface; AppID, Namespace string; ReadyPollInterval, ReconcileInterval time.Duration }` (defaults 100 ms and 30 s)
  - `func New(Options) *Host`
  - `func (h *Host) Add(ctx context.Context, component string, d sandbox.Sandbox, cfg sandbox.Config)` — non-blocking
  - `func (h *Host) Remove(ctx context.Context, component string) error`
  - `func ActorType(namespace, appID, component string) string` → `"dapr.internal."+ns+"."+appID+".sandbox."+component`
- **Behaviour:**
  - **Add** starts a goroutine. It polls `Ready` until it returns `ready=true`, logging `reason` every 5 s, and only then registers the actor type. The goroutine then calls `Reconcile(LiveInstanceIDs())` every `ReconcileInterval`.
  - **Remove** stops the goroutine, unregisters the type (which halts actors and releases each VM through Task 5), then calls `Reconcile([])`.

- [ ] **Step 1: Write failing tests** (fake `actors.Interface`/table from `pkg/actors/table/fake`).
  - `TestRegistersOnlyAfterReady`: the driver reports not ready for 3 polls → no `RegisterActorTypes` before the 4th poll, exactly one after, with type `dapr.internal.default.app1.sandbox.py`.
  - `TestReconcileLoop`: `ReconcileInterval` 10 ms → `Reconcile` called with the live IDs.
  - `TestRemoveReleasesAll`: after `Remove`, `UnRegisterActorTypes` is called with the type, and `Reconcile` with an empty list.
- [ ] **Step 2: Confirm they fail.**
- [ ] **Step 3:** Implement. Replace the Task 3 stub.
- [ ] **Step 4: Confirm they pass:** `go test -race -tags="allcomponents unit" ./pkg/runtime/sandboxhost/ ./pkg/runtime/processor/...` → PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): register sandbox actor types once drivers are ready`.

### Task 7: Runtime API (proto, universal, gRPC)

**Files:**
- Modify: `dapr/proto/runtime/v1/dapr.proto` (import Task 5's `sandbox.proto` + rpcs next to binarystore at :358-364), `pkg/api/grpc/endpoints.go:29,173-177` (allowlist `"sandbox.v1alpha1"`), `pkg/proto/runtime/v1/dapr_tracing.go` (span attributes `component`, `sandbox_id`)
- Create: `pkg/api/universal/sandbox.go`
- Test: `pkg/api/universal/sandbox_test.go`

**Interfaces:**
- Consumes: Task 5's runtime messages and method names, and Task 6's `sandboxhost.ActorType`.
- Produces these rpcs on `service Dapr`:
  - `SandboxExecAlpha1`, `SandboxCreateAlpha1`, `SandboxDeleteAlpha1` (returns `google.protobuf.Empty`)
  - `SandboxSessionStartAlpha1`, `SandboxSessionSendAlpha1`, `SandboxSessionCloseAlpha1`
  - They are implemented as `func (a *Universal) <Rpc>(ctx, *runtimev1pb.<Req>) (*runtimev1pb.<Resp>, error)`, which satisfies `DaprServer` automatically (`pkg/api/grpc/grpc.go:93-94`).
- **Behaviour:**
  - **Target app:** `appId` empty → `a.appID`. When the target is self and the compstore has no such component → `ERR_SANDBOX_NOT_FOUND`.
  - **Routing:** builds `internalsv1pb.NewInternalInvokeRequest(method).WithActor(sandboxhost.ActorType(a.namespace, target, component), sandboxId).WithData(protoBytes).WithContentType("application/x-protobuf")` and calls `a.ActorRouter(ctx)` → `router.Call`.
  - **Timing:** sets `timing.totalUs` from handler entry to response.
  - **Errors:** maps gRPC codes to the error table.
  - **Create:** generates a UUIDv4 and calls `activate`. On `ResourceExhausted` it retries with a fresh ID, up to `maxCreateAttempts = 5`. When all attempts fail → `ERR_SANDBOX_HOST_FULL`.

- [ ] **Step 1: Write failing tests** (fake router recording requests and returning scripted responses).
  - `TestExecRoutesToActorType`: `appId:"pool"` → the actor type is `dapr.internal.default.pool.sandbox.py`, the method is `exec`, and `timing.totalUs>0`.
  - `TestExecSelfMissingComponent`: → `ERR_SANDBOX_NOT_FOUND`.
  - `TestCreateRetriesOnFull`: the router returns `ResourceExhausted` twice, then OK → 3 calls with 3 distinct IDs, and the response ID equals the third.
  - `TestCreateGivesUp`: 5× full → `ERR_SANDBOX_HOST_FULL`.
  - `TestLostMapsToSandboxLost`: `FailedPrecondition` → `ERR_SANDBOX_LOST`.
  - `TestValidation`: empty `sandboxId` or `command` → `ERR_SANDBOX_INVALID_REQUEST`.
- [ ] **Step 2: Confirm they fail.**
- [ ] **Step 3:** Write the proto, run `make gen-proto-runtime`, implement.
- [ ] **Step 4: Confirm they pass:** `go test -tags="allcomponents unit" ./pkg/api/universal/ ./pkg/api/grpc/ ./pkg/proto/runtime/v1/` → PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): alpha runtime API`.

### Task 8: HTTP API

**Files:**
- Create: `pkg/api/http/sandbox.go`
- Modify: `pkg/api/http/http.go:151-169` (append endpoints), `pkg/api/http/endpoints/endpointgroup.go:44-63` (group name `sandbox`)
- Test: `pkg/api/http/sandbox_test.go` (pattern: `binarystore_test.go:104-150`)

**Interfaces:**
- Consumes: Task 7 universal methods via `UniversalHTTPHandler` (pattern: `pkg/api/http/conversation.go:25-80`). `InModifier` fills `component`, `sandboxId` and `sessionId` from chi params, and `appId` from header `dapr-app-id` when present.
- Produces these routes (`v1.0-alpha1`):

  | Method | Path | Universal method |
  |---|---|---|
  | `POST` | `/sandboxes/{component}` | Create |
  | `POST` | `/sandboxes/{component}/{sandboxId}/exec` | Exec |
  | `DELETE` | `/sandboxes/{component}/{sandboxId}` | Delete |
  | `POST` | `/sandboxes/{component}/{sandboxId}/sessions` | SessionStart |
  | `POST` | `/sandboxes/{component}/{sandboxId}/sessions/{sessionId}` | SessionSend |
  | `DELETE` | `/sandboxes/{component}/{sandboxId}/sessions/{sessionId}` | SessionClose |

- [ ] **Step 1: Write failing tests.**
  - `TestSandboxHTTP_ExecJSON`: POST JSON `{"command":"echo","args":["hi"]}` → 200 and a JSON body with `activation` and `timing`.
  - `TestSandboxHTTP_AppIDHeader`: header `dapr-app-id: pool` reaches universal as `appId:"pool"`.
  - `TestSandboxHTTP_UnsafeID`: sandbox ID `a:b%2Fc%E2%9C%93` plus a 300-byte ID both reach universal decoded and unchanged.
  - `TestSandboxHTTP_ErrorStatuses`: full → 429, lost → 410, invalid → 400.
- [ ] **Step 2: Confirm they fail.**
- [ ] **Step 3:** Implement.
- [ ] **Step 4: Confirm they pass:** `go test -tags="allcomponents unit" ./pkg/api/http/ -run Sandbox` → PASS.
- [ ] **Step 5: Commit** with the message `feat(sandbox): alpha HTTP API`.

### Task 9: Integration tests and patch export

**Files:**
- Create: `tests/integration/framework/process/sandbox/sandbox.go` (fake pluggable driver; pattern: `framework/process/statestore/statestore.go:45-110`, including `reflection.Register`)
- Create: `tests/integration/suite/daprd/sandbox/{sandbox.go,process.go,crossapp.go,pluggable.go}`
- Modify: `tests/integration/suite/daprd/daprd.go` (blank import)

**Interfaces:**
- Consumes: the `daprd` framework options `WithResourceFiles`, `WithInMemoryActorStateStore`, `WithPlacementAddresses`, `WithSchedulerAddresses`, `WithSocket` (`framework/process/daprd/options.go`), and placement + scheduler processes as in `suite/daprd/workflow/basic/single.go:57-76`.

- [ ] **Step 1: Write the integration tests.**
  - `process.go`:
    - a single `daprd` with a `sandbox.process` component `py` (`warmPoolSize: 2`);
    - exec `echo hi` on a new ID → 200, `acquirePath` `POOL`;
    - a second exec → the same `activation`;
    - session start `sh -i` → send `X=42` → send `echo $X` (until `42`) → output contains `42`;
    - exec with a stale `expectedActivation` → 410;
    - `POST /sandboxes/py` → an ID that can then exec;
    - an unknown component → 4xx within 5 s.
  - `crossapp.go`:
    - `daprd` A (app `pool`) hosts `py`; `daprd` B (app `caller`) has no sandbox component;
    - B exec with header `dapr-app-id: pool` → 200;
    - A's process count rises, B's doesn't.
  - `pluggable.go`:
    - the fake driver over a socket, with `type: sandbox.<socketName>`;
    - the fake reports not ready for 1 s → exec during that time returns an error body with an `ERR_` code within 5 s (no hang, `daprd` still running);
    - after ready → 200;
    - killing the fake → the next exec gets 410 within 5 s.
- [ ] **Step 2: Run them, plus the existing actor tests.**
  - `go test -race -tags integration ./tests/integration -focus daprd/sandbox` → PASS. They can only pass once Tasks 2-8 are in.
  - `go test -tags="allcomponents unit" ./pkg/actors/... ./pkg/runtime/wfengine/...` → PASS. This proves user-defined actors and workflows are unchanged (spec anti-goal).
- [ ] **Step 3: Commit** with the message `test(sandbox): integration tests`. Then run `scripts/export-patches.sh` from the repo root and commit `proposal/dapr-poc-patches/` in this repo with the message `chore: export dapr PoC patches`.

---

## Part B: microVM driver (`driver/`)

### Task 10: Driver core (pool, service, fake backend)

**Files:**
- Create: `scripts/sync-proto.sh` (copies `common.proto` and `sandbox.proto` from `upstream/dapr/dapr/proto/components/v1/`), `scripts/check-proto-sync.sh` (`diff -r`, non-zero on drift)
- Create: `driver/Cargo.toml` (crate `dapr-sandbox-microvm`, binary `dapr-sandbox-microvm`; feature `microsandbox` default-on gating the real backend), `driver/build.rs` (tonic-build plus a file descriptor set for reflection)
- Create: `driver/src/{main.rs,service.rs,pool.rs,config.rs,backend.rs}`
- Test: `driver/tests/pool.rs`, `driver/tests/service.rs`

**Interfaces:**
- Consumes: Task 2 proto.
- Produces:
  - `config.rs`: `Config::from_properties(&HashMap<String,String>) -> Result<Config, ConfigError>`. Same names, defaults and errors as Go `ParseConfig`; `template` is `Template::{Snapshot, None}`.
  - `backend.rs`:
    - `#[async_trait] trait VmBackend: Send+Sync+'static`
    - `prepare_template(&self, cfg:&Config, name:&str) -> Result<()>`
    - `spawn(&self, cfg:&Config, name:&str, from_template: Option<&str>) -> Result<()>`
    - `exec(&self, name:&str, spec: ExecSpec) -> Result<ExecResult>`
    - `session_start(&self, name:&str, spec: ExecSpec) -> Result<String>`
    - `session_send(&self, name:&str, session:&str, input:&[u8], until:&[u8], timeout: Duration) -> Result<ExecResult>`
    - `session_close(&self, name:&str, session:&str) -> Result<()>`
    - `destroy(&self, name:&str) -> Result<()>`
    - `list(&self, prefix:&str) -> Result<Vec<String>>`
    - `ExecSpec`/`ExecResult` mirror the proto. `BackendError::NotFound` maps to gRPC `NOT_FOUND`.
    - `pub mod fake { pub struct FakeBackend }` with injectable delays and failures.
  - `pool.rs`:
    - `Pool::new(component:&str, cfg: Config, backend: Arc<dyn VmBackend>) -> Arc<Pool>`
    - `start(self:&Arc<Self>)` — background: delete leftovers from `list(prefix)`, prepare the template, fill to `warm_pool_size` with `refill_concurrency` workers
    - `ready() -> (bool, String)`
    - `acquire() -> Result<Acquired{instance_id, path, elapsed}, PoolError::HostFull>` — pool hit → `Pool`; empty pool → inline `spawn` → `Restore` (template) or `Boot` (`template=none`); each claim triggers a refill
    - `release(&str)` (spawned destroy, returns immediately), `reconcile(&[String]) -> u32`, `shutdown()` (destroy all)
    - **VM names:** `format!("dsb-{}-{:06}", &hex(sha256(component))[..8], counter)`. They never derive from sandbox IDs.
  - `service.rs`: the tonic `Sandbox` service. It keeps `Pools` keyed by the `x-component-instance` request header; `Init` builds and starts a `Pool`.
  - `main.rs`:
    - socket from `--socket` or `DAPR_COMPONENT_SOCKET`, default `/tmp/dapr-components-sockets/microvm.sock`;
    - remove a stale socket file; serve with `tonic-reflection`;
    - on SIGTERM/SIGINT → `shutdown()` on every pool, then exit;
    - `--selftest <image>` boots one VM, runs `echo ok`, prints the timings and exits 0.

- [ ] **Step 1: Write failing tests** (`FakeBackend`; no hypervisor).
  - `pool_fills_then_ready`: `warmPoolSize 3` → `ready()` false until 3 spawns, then true.
  - `acquire_pool_then_restore`: size 1, two quick acquires → `Pool` then `Restore`.
  - `template_none_reports_boot`.
  - `host_full`: `maxSandboxesPerHost 1` → the second acquire → `HostFull`.
  - `release_is_async`: fake destroy sleeps 2 s → `release` returns in < 20 ms.
  - `startup_reaps_leftovers`: the fake lists 2 names with the pool prefix → both destroyed before ready.
  - `reconcile_releases_unknown`.
  - `shutdown_destroys_all`.
  - `names_are_valid`: 1000 names all match `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
  - Service test over a temp Unix socket with a tonic client:
    - two components (`x-component-instance` a and b) get separate pools;
    - `Exec` on an unknown instance → `NOT_FOUND`;
    - `Acquire` when full → `RESOURCE_EXHAUSTED`.
- [ ] **Step 2: Confirm they fail:** `cargo test --manifest-path driver/Cargo.toml --no-default-features` → FAIL.
- [ ] **Step 3:** Run `scripts/sync-proto.sh`, then implement.
- [ ] **Step 4: Confirm they pass:** the same command plus `scripts/check-proto-sync.sh` → PASS, exit 0.
- [ ] **Step 5: Commit** with the message `feat(driver): pool, gRPC service and fake backend`.

### Task 11: microsandbox backend

**Files:**
- Create: `driver/src/microsandbox_backend.rs`
- Test: `driver/tests/microsandbox_live.rs` (all `#[ignore]`; run explicitly on a machine with a hypervisor)

**Interfaces:**
- Consumes: the `VmBackend` trait (Task 10); microsandbox 0.7.11:
  - `Sandbox::builder(..).image().cpus().memory().disable_network().replace().create()/create_detached()`
  - `exec_with(cmd, |o| o.args().envs().cwd().timeout().stdin_bytes())`
  - `exec_stream_with(.., |o| ...stdin_pipe())` + `ExecHandle::{recv,take_stdin}`
  - `Snapshot::builder(name).from_sandbox(src).full().create()`
  - `Sandbox::restore(snap).name(n).cow_memory().restore()`
  - `destroy()`, `Sandbox::get`
- Produces: `MicrosandboxBackend::new(msb_home: PathBuf) -> Result<Self>`, implementing `VmBackend`. It holds `DashMap<String, Sandbox>` and session handles.
- **prepare_template:**
  1. boot `<name>-tpl`;
  2. run `sh -c "$setup"` if one is set;
  3. take a full snapshot `<name>-snap`;
  4. destroy the template VM.
- **spawn:**
  - with a template → CoW restore;
  - without → `create_detached` + setup.
- **Exec:** cap 4 MiB, set `timed_out` when the timeout kills it.
- **Sessions:** the stream handle is kept per session id; send writes input, then reads events until `until` or the timeout.

- [ ] **Step 1: Write the live tests (run on the Mac).**
  - `live_selftest`: `python:3.12-slim` → spawn from template, exec `echo ok` → stdout `ok\n`.
  - `live_session_turn2`: `python3 -u -i -q`, send `x = 42\n` with until `>>> `, then `print(x * 2)\n` → stdout contains `84`.
  - `live_restore_many`: restore 8 concurrently from one snapshot → all exec `echo` OK. Print p50 restore ms.
  - `live_timeout_and_cap`: `python3 -c "while True: pass"` with `timeout_ms 500` → `timed_out`; `head -c 10485760 /dev/zero` → 4194304 bytes, `truncated`.
  - `live_shutdown_no_orphans`: after `shutdown()`, `pgrep -f "msb machine"` finds none of this pool's names.
- [ ] **Step 2: Confirm they fail:** `cargo test --manifest-path driver/Cargo.toml -- --ignored` → FAIL (unimplemented).
- [ ] **Step 3:** Implement. `main.rs` selects this backend under the default feature, with `MSB_HOME` defaulting to `$XDG_STATE_HOME/dapr-sandbox-microvm/msb`, falling back to `~/.local/state/...`.
- [ ] **Step 4: Confirm they pass:** the same command on the Mac → PASS. Then `cargo run --release --manifest-path driver/Cargo.toml -- --selftest python:3.12-slim` prints the timings.
- [ ] **Step 5 (optional stretch):** try a live pre-imported interpreter in the template. Start it from the image entrypoint, outside microsandbox jobs (spec: Dependencies, item 2). Record what happened in `results/mac/notes.md` whether or not it works.
- [ ] **Step 6: Commit** with the message `feat(driver): microsandbox backend with snapshot-restored warm pool`.

---

## Part C: Mac self-hosted

### Task 12: Self-hosted demo

**Files:**
- Create: `demo/components/{py-fast.yaml,py-restore.yaml,py-boot.yaml,actorstore.yaml}`
  - `py-fast`: `type: sandbox.microvm`, `image: python:3.12-slim`, `warmPoolSize: 8`, `refillConcurrency: 4`
  - `py-restore`: `warmPoolSize: 0`
  - `py-boot`: `warmPoolSize: 0`, `template: none`
  - `actorstore`: `state.in-memory` with `actorStateStore: "true"`
- Create: `demo/run-selfhosted.sh` (`up` / `down`), `demo/smoke.sh`, `demo/README.md`

**Interfaces:**
- Consumes: `dapr init --runtime-version 1.19.0` (placement and scheduler in OrbStack Docker), `upstream/dapr/dist/darwin_arm64/release/daprd`, and the driver binary.
- **`run-selfhosted.sh up`:**
  1. build `daprd` and the driver;
  2. start the driver on `/tmp/dapr-components-sockets/microvm.sock`;
  3. start `daprd --app-id sandbox-host --resources-path demo/components --dapr-http-port 3500 --placement-host-address localhost:50005 --scheduler-host-address localhost:50006`;
  4. wait until `GET /v1.0/healthz` succeeds and `POST /v1.0-alpha1/sandboxes/py-fast` returns 200;
  5. write PIDs to `demo/.pids`.
- **`down`:** stops both and checks no `msb machine` processes are left.

- [ ] **Step 1: Write `demo/smoke.sh`.** It exits non-zero on any failed assertion:
  - exec `echo hi` on ID `smoke-1` → `acquirePath` `POOL`;
  - session `python3 -u -i -q`: `x = 42` then `print(x * 2)` → `84`;
  - a stale `expectedActivation` → HTTP 410;
  - delete → 200.
- [ ] **Step 2:** `demo/run-selfhosted.sh up && demo/smoke.sh` → all assertions pass. Then `demo/run-selfhosted.sh down` → "no leftover VMs".
- [ ] **Step 3: Commit** with the message `feat(demo): self-hosted sandbox demo on macOS`.

### Task 13: Benchmark harness

**Files:**
- Create: `bench/Cargo.toml` (crate `sandbox-bench`), `bench/src/{main.rs,run.rs,stats.rs,report.rs}`
- Test: `bench/tests/stats.rs`, `bench/tests/report.rs`

**Interfaces:**
- Consumes: the HTTP API (Task 8) and the `timing` fields.
- **`run` subcommand:**
  - `run --dapr-url URL --app-id ID --component C --path a|b|c|d --mode sequential --count N | --mode rate --rate R --host-pods H --duration S --out FILE.csv`
  - **Paths:** a/c/d use a fresh UUID per request. b reuses one warmed ID. a uses component `py-fast`, c `py-restore`, d `py-boot`.
  - **`--workload`:**
    - `echo` (default; the gate);
    - `python` (`python3 -c 'print(1)'`);
    - `session-import` (session start `python3 -u -i -q`, then one turn `import json, decimal, email.parser; print(1)` with until `>>> `; reports the time to that turn's output).
    - The last two are reported, never gated (spec: Dependencies, item 2).
  - **Rate mode:** open loop, firing every `1/(R·H)` s regardless of completions.
  - **CSV columns:** `path,workload,mode,rate,seq,status,acquire_path,total_us,host_us,activate_us,exec_us,client_us`.
- **`sweep`:** `--rates 10,20,40,80,160` runs path a in rate mode for each.
- **`report --in DIR --out DIR`:** writes `summary.md` (p50/p99 per path and mode, a breakdown table, and the count of path (a) requests whose `acquire_path != POOL`) and `rate-sweep.svg` (plotters, p50/p99 against rate).
  - It prints `GATE PASS` or `GATE FAIL`: path (a) `workload=echo` rows with `acquire_path==POOL` must have p99 `total_us` < 100000 in both `sequential` and `rate=20`.
  - Rows with any other `acquire_path` are reported but excluded from the gate. If they are more than 1% of the rows, it prints a warning.
- `stats.rs`: `percentile(sorted:&[u64], p:f64) -> u64` (nearest-rank).

- [ ] **Step 1: Write failing tests.**
  - `percentile`: of `1..=100` → p50=50, p99=99, p100=100; empty → panics with a message.
  - `report_gate`:
    - a fixture CSV where POOL p99 is 90 ms → `GATE PASS`;
    - one at 120 ms → `GATE FAIL`;
    - a fixture with 5% RESTORE rows → warning printed, and those rows excluded.
- [ ] **Step 2: Confirm they fail:** `cargo test --manifest-path bench/Cargo.toml` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4: Confirm they pass**, then run a smoke against the demo:
  - `cargo test` → PASS;
  - with the demo up, `cargo run --release --manifest-path bench/Cargo.toml -- run --dapr-url http://localhost:3500 --app-id sandbox-host --component py-fast --path a --mode sequential --count 20 --out /tmp/x.csv` → 20 rows.
- [ ] **Step 5: Commit** with the message `feat(bench): latency harness with gate report`.

### Task 14: Mac results and checkpoint

**Files:**
- Create: `results/mac/raw/*.csv`, `results/mac/summary.md`, `results/mac/rate-sweep.svg`, `results/mac/notes.md` (machine, macOS version, versions used, commands run)

- [ ] **Step 1: Run the benchmark.**
  - Paths a, b, c and d sequential, `--count 500`;
  - path a with `--workload python` and `--workload session-import`, sequential, `--count 200`;
  - path a at rate 20 for 60 s (`--host-pods 1`);
  - a sweep for 30 s per rate.
- [ ] **Step 2:** `report --in results/mac/raw --out results/mac` → `summary.md` written.
- [ ] **Step 3: Commit** with the message `results: macOS self-hosted latency`.
- [ ] **Step 4: Checkpoint.** If path (a) p99 is ≥ 100 ms on the Mac, or the breakdown shows the exec relay dominating, **stop and report the breakdown to the user before Part D.**

---

## Part D: Kubernetes (kind)

### Task 15: Spike — `/dev/kvm` inside a kind pod (STOP GATE)

**Files:**
- Create: `deploy/kind/lima.yaml`
  - `vmType: vz`, `nestedVirtualization: true`, Ubuntu 24.04 arm64, 8 CPUs, 12 GiB;
  - mounts the repo writable at the same path;
  - provisions Docker and kind.
- Create: `deploy/kind/kind.yaml` — cluster name `sandbox`; 1 control-plane and 2 workers, each with `extraMounts` `/dev/kvm` → `/dev/kvm`.
- Create: `deploy/kind/up.sh` (`brew install lima helm` if missing; create the VM; `kind create cluster`; export the kubeconfig to `$REPO/.kube/kind-sandbox.yaml`), `deploy/kind/down.sh`
- Create: `deploy/kind/kvm-check.yaml` (privileged pod with hostPath `/dev/kvm`)

- [ ] **Step 1:** `deploy/kind/up.sh`, then `limactl shell sandbox ls -l /dev/kvm` → a character device. `kubectl --context kind-sandbox get nodes` → 3 Ready.
- [ ] **Step 2:** Build the driver image for linux/arm64 inside the Lima VM. The image is `deploy/images/driver.Dockerfile`: a multi-stage cargo build with `download-binaries` fetching the linux-aarch64 bundle. Load it with `kind load docker-image dapr-sandbox-microvm:dev --name sandbox`.
- [ ] **Step 3:** Apply `kvm-check.yaml`. Its pod runs `dapr-sandbox-microvm --selftest python:3.12-slim`. `kubectl --context kind-sandbox logs kvm-check` → `ok` plus the timings.
- [ ] **Step 4:** If any step fails, **stop and report** the error and what was tried. Don't change the route without the user.
- [ ] **Step 5: Commit** with the message `feat(kind): Lima + kind with /dev/kvm in pods`.

### Task 16: Dapr on kind with the sandbox pool

**Files:**
- Create: `deploy/kind/build-images.sh`
  - on the Mac: `make build-linux` (all binaries) in `upstream/dapr`;
  - in Lima: `make docker-build TARGET_ARCH=arm64 DAPR_REGISTRY=localhost/dapr DAPR_TAG=dev`, then `kind load` each image.
- Create: `deploy/kind/helm-values.yaml` (`global.registry: localhost/dapr`, `global.tag: dev-linux-arm64`, `global.imagePullPolicy: Never`)
- Create: `deploy/kind/manifests/`
  - `components.yaml`: `py-fast` and `py-restore` (as in the demo), plus `state.in-memory` `actorStateStore` — all with `scopes: [sandbox-pool]`.
  - `sandbox-pool.yaml`:
    - a Deployment, 2 replicas;
    - annotations `dapr.io/enabled`, `dapr.io/app-id: sandbox-pool`, `dapr.io/pluggable-components: microvm`;
    - container `microvm` runs the driver: privileged, hostPath `/dev/kvm`, `MSB_HOME` on an emptyDir;
    - pod anti-affinity across the 2 workers.
  - `caller.yaml`: a Deployment, 1 replica, app-id `sandbox-caller`, running `sleep infinity` with the bench binary in the image.
- Create: `deploy/kind/smoke.sh`

- [ ] **Step 1:** `build-images.sh`, then `helm install dapr upstream/dapr/charts/dapr -n dapr-system --create-namespace --kube-context kind-sandbox -f deploy/kind/helm-values.yaml --wait` → all control-plane pods Ready.
- [ ] **Step 2:** Apply the manifests and wait until both `sandbox-pool` pods are Ready. Their `daprd` logs show the actor type `dapr.internal.default.sandbox-pool.sandbox.py-fast` registered.
- [ ] **Step 3: Write and run `deploy/kind/smoke.sh`.** It execs inside the caller pod against `localhost:3500` with `dapr-app-id: sandbox-pool`:
  - exec `echo hi` on 10 fresh IDs → all 200;
  - both pool pods have served at least one, checked by grepping the driver logs for instance creation.
  - Expected: PASS.
- [ ] **Step 4: Commit** with the message `feat(kind): Dapr sandbox pool on kind`.

### Task 17: Kubernetes gate run

**Files:**
- Create: `results/kind/raw/*.csv`, `results/kind/summary.md`, `results/kind/rate-sweep.svg`, `results/kind/notes.md` (topology, versions, nested virtualization caveat, privileged pods)

- [ ] **Step 1:** From the caller pod, run the same matrix as Task 14 with `--app-id sandbox-pool --host-pods 2`. Copy the CSVs out with `kubectl --context kind-sandbox cp`.
- [ ] **Step 2:** `report --in results/kind/raw --out results/kind` → prints `GATE PASS` or `GATE FAIL`. Record the output verbatim in `summary.md`.
- [ ] **Step 3: Commit** with the message `results: kind latency gate`. If the gate fails, the results say so plainly. Report to the user before Part E.

---

## Part E: Write-up (drafts only, never posted)

### Task 18: Proposal, issue, PR text, README

**Files:**
- Create: `proposal/20261028-BRCS-sandbox-building-block.md`
  - Follows the `dapr/proposals` template sections exactly: Overview; Background; Related Items; Expectations and alternatives; Implementation Details (Design, Feature lifecycle outline, Acceptance Criteria); Completion Checklist.
  - **Acceptance criteria** quote the measured numbers from `results/kind/summary.md` and `results/mac/summary.md`.
  - **Alternatives:** an app on Dapr actors; Kata; agent-sandbox; Azure Container Apps dynamic sessions.
  - **Future work:**
    - flexible profile with save/restore to binarystore (phase 2);
    - cross-cluster, citing dapr/dapr#9128;
    - a Cloud Hypervisor/mshv driver;
    - native Windows/WHP;
    - an unprivileged KVM device plugin;
    - capacity-aware placement.
  - **Limitations:**
    - sandboxes are lost on placement changes;
    - a full host blocks caller-chosen IDs;
    - the template holds a warm disk and page cache only (plus the stretch-goal outcome from Task 11).
- Create: `proposal/issue.md` (≤ 1 page, links the proposal; this is for `dapr/dapr`)
- Create: `proposal/pr.md` (what changed, how to run, results tables, the privileged-pod caveat; ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`)
- Modify: `README.md`
  - quickstart: `demo/run-selfhosted.sh up`, `demo/smoke.sh`, `deploy/kind/up.sh`;
  - where the results live;
  - a Windows note: run inside WSL2 with nested virtualization (pluggable components need Unix sockets); native WHP is future work.

- [ ] **Step 1:** Write the four files. Every number must come from `results/` (spec anti-goal: no figures that weren't measured).
- [ ] **Step 2: Check the numbers.** `grep -o '[0-9.]\+ \?ms' proposal/*.md | sort -u`. Each value must appear in `results/*/summary.md`; fix any that don't.
- [ ] **Step 3:** Run `scripts/export-patches.sh` again, so the patches match the final branch.
- [ ] **Step 4: Commit** with the message `docs: proposal, issue and PR drafts for review`.
