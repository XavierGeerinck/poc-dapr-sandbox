# Dapr Sandbox building block: sub-100ms microVMs orchestrated by Dapr actors

Date: 2026-10-28
Status: design, awaiting review

## Intent

Developers who run untrusted code (AI agents, function execution, notebooks) need one isolated
environment per session. They want it to start fast, behave the same on a laptop and on
Kubernetes, and not require a separate orchestration system. Today the options are Kata
Containers (one VM per pod: about 1.9 s to a running container even with cached images) or a
dedicated sandbox product such as microsandbox, Azure Container Apps dynamic sessions or
kubernetes-sigs/agent-sandbox.

Dapr already solves the hard distributed part of this problem with virtual actors. An actor
has exactly one live instance per ID across the cluster. Every call for that ID is routed to the
pod that owns it, calls run one at a time, and idle actors are cleaned up. Workflows show
that Dapr can host actor types inside the sidecar itself, with no app in the loop.

This project proposes a **Dapr-native Sandbox building block**:
- a sandbox ID is a Dapr actor ID;
- the sandbox is an isolated microVM provided by a pluggable driver (a Dapr component);
- the API is a small alpha HTTP/gRPC surface on the sidecar.

It then proves, with a working proof of concept and measured numbers, that a new sandbox ID can
run its first command in under 100 ms. The proof runs on a developer laptop and on Kubernetes.

The deliverables are drafts for the user to review: a Dapr proposal, an issue, and a
proof-of-concept PR. **Nothing is pushed to GitHub or posted without the user's explicit
go-ahead.**

## Decided with the user

- **Dapr-native, not an app beside Dapr.** The capability lives inside `daprd` as a new building
  block, configured with Component YAML, the same way Workflows live inside the sidecar. It is
  not a standalone service that implements the app-side actor HTTP contract.
- **Drivers behind a contract.** The isolation technology is a replaceable driver. microsandbox
  must not be the centre of the design or leak into the Dapr API.
- **Two profiles, one API.**
  - *Fast profile* (functions, AI agents): fixed environment, template snapshot, warm pool,
    VM thrown away on deactivation. Target: new ID in under 100 ms.
  - *Flexible profile* (notebooks): per-sandbox image/resources, runtime package installs, full
    VM save on deactivation and restore on reactivation. Resume in seconds is acceptable.
- **Scope split.** The proof of concept proves the fast profile end to end with measured numbers.
  The proposal describes the flexible profile, with save/restore as phase 2.
- **Order of work.** First make it work locally on the Mac without Kubernetes (Dapr self-hosted
  mode). Then run it on Kubernetes using kind.
- **Dependencies.** Three layers:
  1. baked into the image;
  2. pre-imported into a template snapshot;
  3. installed at runtime by the sandbox itself.

  Different environments are different sandbox components.

## Goals

1. A proposal document for a new alpha **Sandbox** building block, in the `dapr/proposals`
   template, ready for the user to review and submit.
2. A short issue text for `dapr/dapr` that introduces the idea and links the proposal.
3. A proof-of-concept branch of `dapr/dapr` (based on tag `v1.19.0`) that implements the fast
   profile inside `daprd`, plus a PR description for it.
4. A microVM driver, implemented as a Dapr pluggable component, that runs on macOS (Apple Silicon,
   Hypervisor.framework) and Linux (KVM).
5. A benchmark that reports p50/p99 latency for each path below, on the Mac in self-hosted mode
   and on kind.
   - **Where the gate must pass (decided by the user): on Kubernetes (kind).** That includes the
     cross-pod case, where the caller runs in a different pod from the sandbox host.
   - **The Mac:** numbers are reported but are not the gate.
   - **Load level for the gate (my call):** sequential requests, and a sustained 20 new
     sandboxes per second per host pod.
   - **Beyond the gate:** the results also chart latency against rate, to show where path (a)
     degrades into path (c) as the warm pool drains.

   | Path | Gate |
   |---|---|
   | (a) new sandbox ID served from the warm pool | **p99 < 100 ms** |
   | (b) warm exec on an existing ID | Reported (expected a few ms) |
   | (c) new ID with the warm pool empty, served by snapshot restore | Reported; target < 100 ms, not a gate |
   | (d) new ID with no snapshot (fresh boot) | Reported, for honesty only |
6. A demo a Dapr maintainer can run locally from a README in a few commands.

### The latency metric (must be used exactly)

Measure from the moment the exec request reaches the **caller's** sidecar to the first byte of
output from a trivial command (`echo` or `true`) in the guest. Measure per request, over enough
requests to report p99. Break each number down into:
- Dapr routing;
- activation and pool claim;
- driver/VM handover;
- exec inside the guest.

The README footnote figure that microsandbox advertises ("guest boot on an M1") is **not** this
metric and must not be quoted as if it were.

## Non-goals (for the proof of concept)

- Cross-cluster sandboxes. Dapr placement and scheduler are per cluster. A Dapr maintainer
  confirmed in dapr/dapr#9128 (Oct 2025) that multi-cluster would need a shared scheduler, a flat
  network and a shared trust domain. The proposal lists this as future work.
- The flexible profile's save/restore (phase 2, described in the proposal only).
- Native Windows. Dapr pluggable components use Unix domain sockets, and Dapr's own tests skip
  them on Windows (`pkg/components/pluggable/grpc_test.go:56`). Windows developers use WSL2
  (which also provides `/dev/kvm`). Native WHP support is a proposal item.
- A `/dev/mshv` driver for AKS Pod Sandboxing nodes. libkrun and microsandbox do not support
  mshv. Cloud Hypervisor does, so it is the follow-up driver; it is listed in the proposal.
- Capacity-aware placement changes in Dapr.
- Production hardening, multi-tenancy policy, and network egress policy beyond "no network unless
  asked".

## Anti-goals (failure even if it technically works)

- Claiming sub-100 ms by measuring only guest boot, or by excluding Dapr routing from the number.
- Requiring privileged pods without saying so plainly in the docs and the PR.
- Making a specific VMM or library part of the Dapr API or component contract.
- Touching the user's AKS clusters (`aks-scrydon-ops`, `aks-scrydon-prod`, `klina-aks`). They are
  in the kubeconfig. Never use them; kind only.
- Pushing branches, opening PRs/issues, or posting anywhere without explicit approval.
- Changing Dapr's existing actor behaviour for user-defined actors.

## Verified facts the design relies on

The facts come from reading Dapr v1.19.0 and microsandbox v0.7.11 source, plus web research on
2026-10-28. Paths are relative to each source tree.

**Dapr v1.19.0**
- **Internal actor targets exist and are the model to copy.** `pkg/actors/targets/targets.go`
  defines `targets.Interface` (`InvokeMethod`, `InvokeReminder`, `InvokeTimer`, `InvokeStream`,
  `Deactivate`) and `targets.Factory`. Workflows register internal actor types via
  `table.RegisterActorTypes` from `pkg/runtime/wfengine/backends/actors/actors.go:362`.
- **The placement lookup is local.** The placement table is disseminated to each sidecar. A call
  goes: caller sidecar → local lookup → one mTLS gRPC `CallActor` to the owner sidecar → target.
  There is no per-call round trip to placement.
- **Turn-based locking** is per actor, held for the whole call, and also applies to timers and
  reminders.
- **Idle deactivation** works per actor, using a timer at its idle deadline (default 60 m).
  Deactivation holds the per-type write lock on that host
  (`pkg/actors/targets/app/factory.go:206-233`), so a slow deactivation blocks new activations of
  that type on that host. The sandbox target must release VMs asynchronously.
- **An actor state store is still required to host actors.** Without one, hosting starts
  suspended (`pkg/actors/actors.go:214-223`). Calling actors hosted elsewhere still works.
- **No capacity awareness in placement.** It uses consistent hashing (v1) or rendezvous hashing
  (v2, scheduler-served, opt-in in 1.19). There are no weights and no max actors per host. A
  failing app health check removes all of a host's actor types.
- **`daprd` is built with `CGO_ENABLED=0` by default** (Makefile line 24). VMM libraries therefore
  cannot be linked into `daprd`, so drivers are pluggable components.
- **Pluggable components** talk gRPC over Unix sockets (`DAPR_COMPONENTS_SOCKETS_FOLDER`).
  Existing contracts live in `dapr/proto/components/v1/` (state, pubsub, bindings, secretstore).
- **A binary store alpha API exists:** `/v1.0-alpha1/binarystore/{name}/{fileName}`. It is a
  candidate for phase 2 save/restore.
- **New building blocks start as a proposal PR** to `dapr/proposals`. The file is named
  `YYYYMMDD-FLAGS-description.md`, with flags B (building block), R (runtime), C (components),
  I (CLI) and S (SDKs).

**microsandbox v0.7.11** (a candidate implementation for the first microVM driver)
- **The Rust SDK API** is `Sandbox::builder(name).image(..).cpus(..).memory(..).create()`, then
  `exec` / `exec_stream_with`, `stop`, and `remove`/`destroy`. There is also
  `connect_or_create`. The 0.2-era `PythonSandbox` API in the original idea does not exist.
- **Process model.** Each `exec` is a fresh guest process. Interpreter state survives only inside
  a long-lived process; the SDK offers detached jobs with stdin attach (32 per sandbox).
- **One `msb` child process per VM**, which runs the VMM (libkrun). There is no daemon. State
  lives under `$MSB_HOME`, which is configurable.
- **Backends:** KVM on Linux, Hypervisor.framework on macOS (Apple Silicon only), and WHP on
  Windows (preview). There is no mshv support.
- **The cold create path** includes:
  - image resolution;
  - ext4 formatting of the writable disk;
  - a SQLite write;
  - an `msb` process spawn;
  - waiting for the guest agent.

  None of these may sit on the fast path.
- **Snapshots.** Full snapshot/restore with copy-on-write memory, and live fork, exist. No
  timings are published. Restore needs the same CPU count and memory as the capture.
- **Kubernetes.** The documented setup is a privileged pod with hostPath `/dev/kvm` and
  `MSB_HOME` on an emptyDir. A KVM device plugin is suggested but untested.
- **Networking** is a user-space smoltcp stack; no NET_ADMIN or TUN is needed.

**Local environment (checked on the user's Mac)**
- **Hardware and tools.** Apple M4 Pro, macOS 27.0.1. Installed: Rust/cargo, Docker (OrbStack),
  kind, kubectl, the Dapr CLI, and Apple `container` 1.1.0. **Go is not installed.**
- **No `/dev/kvm` in the existing Linux VMs.** OrbStack's Docker VM has none
  (`docker run --device /dev/kvm` fails). Apple `container --virtualization` starts the guest at
  EL2, so nested virtualization works in hardware, but its kernel has no KVM, so there is no
  `/dev/kvm` either.

## The design

### User-facing shape

A sandbox component, fast profile (illustrative; field names are the builder's to refine):

```yaml
apiVersion: dapr.io/v1alpha1
kind: Component
metadata:
  name: python-fast
spec:
  type: sandbox.microvm        # pluggable component registered under this name
  version: v1
  metadata:
    - name: image
      value: ghcr.io/example/agent-python:1.0
    - name: cpus
      value: "1"
    - name: memoryMiB
      value: "512"
    - name: setup               # run once, then snapshotted into the template
      value: python3 -c "import json, re"
    - name: warmPoolSize
      value: "4"
    - name: maxSandboxesPerHost
      value: "32"
    - name: idleTimeout
      value: 5m
```

API (alpha):
- `POST /v1.0-alpha1/sandboxes/{component}/{id}/exec`: one-shot command. The first call for an
  unknown ID activates it.
- `POST /v1.0-alpha1/sandboxes/{component}`: create a sandbox with an ID chosen by the sidecar. It
  returns the ID. See "Full hosts" below.
- Long-lived sessions (a process kept alive between calls that you can write to and read from) for
  agents and notebooks. The exact endpoints are the builder's call, but the proof of concept
  needs at least one session type so that "turn 2 sees turn 1's variable" works.
- `DELETE /v1.0-alpha1/sandboxes/{component}/{id}`: explicit release.
- Every request takes an optional **target app ID**, for example a `dapr-app-id` header. It
  defaults to the caller's own app ID. See "Hosting model" below.
- Every response carries the sandbox's **activation token**. See "Lost sandboxes" below.
- gRPC equivalents on the Dapr runtime API.

### Hosting model (my call, after review)

Dapr only lets an app host internal actor types it owns: `dapr.internal.<namespace>.<appID>.*`
(`pkg/actors/hostauthz/hostauthz.go:42-100`). So the sandbox actor type is
`dapr.internal.<namespace>.<hostAppID>.sandbox.<component>`.

- **On Kubernetes:** sandboxes run in a **dedicated host deployment**, for example app ID
  `sandbox-pool`.
  - **Privileges:** only those pods load the sandbox component (via component `scopes`), get
    `/dev/kvm`, and need privileges.
  - **Callers:** any other app in the namespace calls them through its own sidecar by passing
    the target app ID. This works the same way as calling actors hosted by another app, and it
    mirrors cross-app workflows.
  - **Why:** this keeps the Kata-like property that untrusted code only runs on a known,
    isolated set of pods.
- **Self-hosted (local dev):** one `daprd` both hosts and calls, and the target app ID defaults
  to itself.

### Lost sandboxes (my call, after review)

Dapr halts actors whenever placement changes for them:
- the placement stream drops (`HaltAll`, `pkg/actors/internal/placement/loops/placement/placement.go:428`);
- the hash ring moves because a host pod joins or leaves, for example on scale-out, a rolling
  update or a failed health check (`loops/disseminator/order.go:102`).

For a sandbox that means its VM and sessions are gone, and the next call silently gets a fresh
sandbox. The PoC does not try to make VMs survive a halt. Instead, it makes the loss visible
without adding a state write to the fast path:
- **Activation token.** Every response includes the sandbox's activation token: a new value per
  activation, like Dapr 1.19's `Dapr-Actor-Activation-Id`.
- **Checking it.** A caller may send the token back on later calls. If the sandbox was recreated
  in the meantime, the call fails with a distinct "sandbox lost" error instead of running in a
  fresh VM.
- **Callers that don't send it** get today's virtual-actor behaviour.
- **Documentation.** The proposal documents which events cause loss. The flexible profile's
  save/restore (phase 2) is the real answer for state that must survive.

### Full hosts (my call, after review)

Hashing pins a caller-chosen ID to one host. If that host is at `maxSandboxesPerHost`, retrying
the same ID keeps failing until capacity frees up. Two answers:
- **Sidecar-chosen IDs.** `POST /v1.0-alpha1/sandboxes/{component}` picks a random ID and tries
  to activate it. On `ResourceExhausted` it picks a new ID, which most likely hashes to a
  different host, up to a small bounded number of tries.
  - This needs no change to placement.
  - It is the recommended path for "give me a fresh sandbox" (functions, agents).
- **Caller-chosen IDs** keep the limitation, and the proposal documents it.

### Inside `daprd`

1. **Component category `sandbox`.** It is loaded like the other categories. A pluggable component
   can register as `sandbox.<name>`.
2. **Contract** `dapr/proto/components/v1/sandbox.proto` between `daprd` and drivers. It is
   VMM-agnostic and covers at least:
   - init/config;
   - acquire (claim from pool or create);
   - exec (streaming output);
   - sessions;
   - release;
   - readiness;
   - capacity/stats.
3. **Internal actor type per sandbox component.** It lives in `pkg/actors/targets/sandbox`,
   modelled on the workflow targets, named
   `dapr.internal.<namespace>.<hostAppID>.sandbox.<component>` (see "Hosting model").
   - **Activation:** acquire from the driver.
   - **`InvokeMethod`:** exec or session operations.
   - **Deactivation:** release the VM asynchronously, never blocking the per-type lock.
4. **Readiness before placement.** `daprd` registers the sandbox actor type with placement only
   after the driver reports that its template and warm pool are ready. No sandbox lands on a pod
   that is still building.
5. **Admission.** When a host is at `maxSandboxesPerHost`, activation fails fast with a retryable
   error (HTTP 429/503 style, gRPC `ResourceExhausted`/`Unavailable`). Callers rely on Dapr
   resiliency policies. Dapr placement does not change.
6. **Built-in `sandbox.process` driver** (no isolation; runs commands as host processes). Used for
   unit/integration tests and CI, and as the reference implementation of the contract.

### The microVM driver (pluggable component, in this repo)

- Rust, implementing `sandbox.proto` over a Unix socket.
- **First implementation: libkrun through microsandbox's Rust SDK.** It already handles OCI
  images, the guest agent, snapshot/restore and fork on macOS and Linux. It is one driver behind
  the contract, not the design. Firecracker and Cloud Hypervisor are named follow-ups.
- **On startup:**
  1. pull or resolve the image;
  2. boot one VM;
  3. run `setup`;
  4. snapshot it as the template;
  5. fill the warm pool by restoring from the template;
  6. report ready.
- **Pool behaviour.** After each claim, the pool refills in the background. When the pool is
  empty, activation restores from the template on demand (path c).
- **Fallback.** If snapshot/restore turns out unusable or too slow in microsandbox, fall back to
  a pool of fresh-booted VMs (still sub-100 ms on the claim path). Record this as a finding.
- **Orphan cleanup.** On startup and periodically, the driver reaps VMs that `daprd` no longer
  owns. This covers cases where deactivation never reached the driver.

### Latency budget for path (a) (target, to be verified)

| Step | Budget |
|---|---|
| Caller → local sidecar | < 1 ms |
| Sidecar → owner sidecar (gRPC; 0 if same pod) | ~1 ms |
| Activation + pool claim | < 2 ms |
| Handover to guest agent + exec of `echo` | a few ms |
| **Total** | **p50 ~5–15 ms; p99 < 100 ms** |

### Dependencies

1. **Image** (default): built with a normal Dockerfile.
2. **Template snapshot (my call, after review):** `setup` commands are processes that exit, and
   microsandbox refuses a full snapshot while a job is running
   (`crates/runtime/lib/runner/control/executor.rs:311-322`). So the PoC template only guarantees
   a **warm disk and page cache**, not a live interpreter with libraries already imported. Neither
   the PoC nor the proposal may claim more than that.
   - **Stretch goal:** put a live, pre-imported interpreter into the template. It would be
     started by the image entrypoint, outside microsandbox's job mechanism, with its own in-guest
     session protocol. Try this only if it fits; record the outcome either way.
   - **Realistic first commands.** Besides the `echo` gate, the benchmark reports two of them on
     a new sandbox: `python3 -c 'print(1)'`, and a first session turn that imports a mid-size
     library. That shows the real cost an agent would see.
3. **Runtime install:** allowed. It lives on the sandbox's writable disk, is lost on deactivation
   in the fast profile, and is persisted in the flexible profile (phase 2).

### Flexible profile (proposal only, phase 2)

- **Per-sandbox spec.** An explicit create (`PUT /v1.0-alpha1/sandboxes/{component}/{id}` with a
  spec) for a custom image or resources. The spec is stored in actor state, so any pod can
  reactivate it.
- **Save and restore.** Full VM save (disk + memory) on deactivation to the binary store, and
  restore on activation.
- **Caveats:**
  - open network connections don't survive;
  - restoring on a different CPU type may fail, in which case it falls back to disk-only restore.

## Environments and phases

1. **Phase 1, Mac self-hosted.**
   - **Dapr control plane:** `dapr init` (placement, scheduler and Redis run in OrbStack Docker).
   - **Processes:** a locally built `daprd` from the PoC branch, and the microVM driver using
     Hypervisor.framework.
   - **Requires:** installing Go. It must match the version in Dapr v1.19.0's `go.mod`.
   - **Exit:** the benchmark reports all four paths, and the demo works. The Mac is not the
     gate, but a clear miss here is worth reporting to the user before starting phase 2.
2. **Phase 2, kind on Linux with KVM.**
   - **Host:** a Linux VM on the Mac with nested virtualization and a KVM-enabled distro kernel.
     My call is Lima with the `vz` backend, `nestedVirtualization: true` and Ubuntu. Docker and
     kind run inside it.
   - **KVM in pods:** `/dev/kvm` is mounted into the kind node via `extraMounts`, then into pods.
     The proof of concept may use the privileged + hostPath pattern, and must say so plainly.
     An unprivileged device-plugin route is documented as a follow-up.
   - **Dapr control plane:** installed with Helm, using images built from the PoC branch.
   - **Risk:** the first step of phase 2 is a spike to confirm `/dev/kvm` exists and a VM boots
     inside a kind pod. If Lima can't provide it, stop and report back before choosing another
     route.
   - **Exit:** the benchmark reports the same four paths on kind, plus a cross-pod case where the
     caller and the owner of the sandbox are on different pods.
     - **The gate:** path (a) at p99 < 100 ms, both sequentially and at 20 new sandboxes per
       second per host pod.
     - **Topology:** at least two host pods plus a separate caller deployment, so the
       hosting model and cross-pod routing are both exercised.
     - **If it fails,** the results say so plainly.

## Deliverables, where they live

| Deliverable | Location |
|---|---|
| Proposal draft (`dapr/proposals` template; flags B, R, C, S) | This repo, `proposal/` |
| `dapr/dapr` issue text | This repo, `proposal/issue.md` |
| PR description | This repo, `proposal/pr.md` |
| `dapr/dapr` PoC branch | A local clone of `dapr/dapr` at `v1.19.0`, branch `feat/sandbox-building-block`. Exact location is the builder's call, but it must stay reviewable locally and must not be pushed. |
| microVM driver | This repo |
| Demo scripts, kind config and Kubernetes manifests | This repo |
| Benchmark harness | This repo |
| Results | This repo, `results/`, with the raw data |

## Left to the builder

- Exact endpoint names, proto message shapes and the session API design, within the constraints
  above.
- How the PoC branch is laid out on disk relative to this repo.
- The benchmark tool and statistics method (it must report p50/p99 and the per-step breakdown).
- Pool sizing defaults.
- How the driver reaps orphan VMs.
- Whether `sandbox.process` lives in `dapr/dapr` or `components-contrib` for the PoC; in-tree is
  fine for a PoC.

## Risks

- **Snapshot restore speed** in microsandbox is unpublished. Path (c) may miss 100 ms. Path (a)
  does not depend on it.
- **Exec round-trip latency** through microsandbox's agent relay is unknown. It is the main
  unknown in path (a).
  - **If path (a) misses the gate because of it, stop and report** (my call, after review),
    with the per-step breakdown.
  - **Don't decide alone.** Do not fork microsandbox, write a custom guest agent, or switch VMMs
    without the user's decision.
- **Nested virtualization in kind on the Mac** is unverified; see the phase 2 spike.
- **Upstream reception.** Dapr maintainers may prefer this outside the runtime. The proposal
  should state the alternatives honestly: an app on Dapr actors, or Workflows-style internal
  actors.
