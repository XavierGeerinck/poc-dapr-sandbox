# [PoC] Sandbox building block (alpha): microVM sandboxes as internal actors

*Draft PR description for the proof-of-concept branch `feat/sandbox-building-block` of
dapr/dapr, based on `v1.19.0`. Local draft, not opened.*

# Description

This branch is a proof of concept for an alpha **Sandbox** building block. The proposal is
`proposal/20261028-BRCS-sandbox-building-block.md` (a dapr/proposals PR, not yet opened).

**What a sandbox is**
- An isolated environment in which an app runs commands it does not trust.
- Each sandbox ID is a Dapr actor ID of an internal actor type,
  `dapr.internal.<namespace>.<hostAppID>.sandbox.<component>`. daprd hosts that type itself,
  as it hosts workflow actors.
- The VM comes from a driver: a component in a new `sandbox` category, usually a pluggable one.

**Scope.** The branch implements the fast profile end to end: a warm pool, a template
snapshot, and VMs thrown away on deactivation.

**Where the driver lives.** The microVM driver used for the measurements (Rust, microsandbox
0.7.11) is not part of this PR; it lives in the companion repository.

## What changed

15 commits on top of `v1.19.0`:

| Area | Files | Commits |
|---|---|---|
| Driver contract and components | `dapr/proto/components/v1/sandbox.proto`, `pkg/components/sandbox/` (metadata, pluggable client, registry), `pkg/components/category.go`, `pkg/runtime/registry/` | component contract and pluggable client; register the component category |
| Reference driver | `pkg/components/sandbox/process/`, `cmd/daprd/components/sandbox_process.go` | built-in `sandbox.process` driver for tests; per-send session output cap |
| Actor target | `pkg/actors/targets/sandbox/` | internal actor target backed by sandbox drivers; context-aware turn wait |
| Hosting | `pkg/runtime/sandboxhost/`, `pkg/runtime/processor/sandbox/`, `pkg/runtime/processor/processor.go`, `pkg/runtime/compstore/sandbox.go` | register sandbox actor types once the driver reports ready; reconcile orphan VMs |
| API | `dapr/proto/runtime/v1/sandbox.proto`, `dapr/proto/runtime/v1/dapr.proto`, `pkg/api/universal/sandbox.go`, `pkg/api/grpc/endpoints.go`, `pkg/api/http/sandbox.go`, `pkg/messages/` | alpha runtime API (gRPC) and HTTP API; error codes; lost-sandbox and not-ready handling |
| ACL | `pkg/api/grpc/workflow_acl.go` | let other apps invoke hosted sandbox actors under mTLS; classify sandbox types before workflow suffixes |
| Tests | `tests/integration/suite/daprd/sandbox/`, `tests/integration/framework/process/sandbox/` | integration tests with the process driver, a fake pluggable driver, and two apps |

### API

All routes are under `/v1.0-alpha1/sandboxes`, with gRPC equivalents `Sandbox*Alpha1`:

| Route | What it does |
|---|---|
| `POST /{component}` | create, with a sidecar-chosen ID |
| `POST /{component}/{sandboxId}/exec` | run a one-shot command |
| `DELETE /{component}/{sandboxId}` | release the sandbox |
| `POST /{component}/{sandboxId}/sessions` | start a session |
| `POST /{component}/{sandboxId}/sessions/{sessionId}` | send to a session |
| `DELETE /{component}/{sandboxId}/sessions/{sessionId}` | close a session |

**Request and response conventions**
- **Target app.** `dapr-app-id` (or `appId` in the body) selects the host app.
- **Activation token.** Every response carries one, and `expectedActivation` turns a lost
  sandbox into 410 `ERR_SANDBOX_LOST`.
- **Full host.** It answers 429 `ERR_SANDBOX_HOST_FULL`.

### The one change to existing behaviour

**The v1.19 rule.** `validateSameAppInternalActor` denies calls from other app IDs to any
`dapr.internal.*` actor under mTLS. That blocked every cross-app call to a host's sandboxes;
on kind the symptom was 500 `ERR_SANDBOX_EXEC`.

**The patch** (commits f220cbd5d and b0ce41dac):
- **What it admits.** Callers from the same namespace, but only for this daprd's own sandbox
  prefix: `dapr.internal.<ns>.<appID>.sandbox.` plus a non-empty component.
- **What stays the same:**
  - reminders on those types are still same-app;
  - other namespaces and other apps' sandbox types are still denied;
  - workflow access policies still apply.
- **Ordering.** Sandbox types are classified before the workflow and activity suffix check.

**This needs a security review.** See also the known limitations below: there is no
per-sandbox owner binding yet.

**Unchanged.** Nothing else changes for apps that load no `sandbox` component, and user-defined
actors are not affected.

## How to run

### Tests

All of these passed on macOS arm64 at b0ce41dac on 2026-10-29, with Go 1.26.10.

```sh
export GOTOOLCHAIN=go1.26.10
go test -count=1 -tags="allcomponents unit" \
  ./pkg/actors/targets/sandbox/ ./pkg/api/universal/ ./pkg/api/http/ ./pkg/api/grpc/ \
  ./pkg/runtime/sandboxhost/ ./pkg/components/sandbox/... \
  ./pkg/runtime/processor/ ./pkg/runtime/processor/sandbox/ \
  ./pkg/runtime/compstore/ ./pkg/resiliency/ ./pkg/components/ ./pkg/messages/... \
  ./pkg/diagnostics/... ./cmd/daprd/...
go test -count=1 -tags integration ./tests/integration -v -focus daprd/sandbox
```

- **Integration suites:** `daprd/sandbox/process`, `daprd/sandbox/pluggable` and
  `daprd/sandbox/crossapp`. The cross-app suite checks:
  - a caller app execs in a pool app's sandbox;
  - the processes run in the pool app only;
  - a stale activation is lost across apps;
  - a sandbox type the pool app does not host answers "retry later".
- **Not run:** the full unit suite of the repository and the e2e suite.

### Self-hosted demo (macOS, Apple silicon)

This uses the companion repository, with this branch checked out at `upstream/dapr`:

```sh
demo/run-selfhosted.sh up && demo/smoke.sh && demo/run-selfhosted.sh down
```

### Kubernetes (kind; Linux with `/dev/kvm`, or macOS through Lima)

This also uses the companion repository:

```sh
export KUBECONFIG=$PWD/.kube/kind-sandbox.yaml
deploy/kind/up.sh
deploy/kind/build-images.sh
helm install dapr upstream/dapr/charts/dapr -n dapr-system --create-namespace \
  --kube-context kind-sandbox -f deploy/kind/helm-values.yaml --wait
kubectl --context kind-sandbox apply -f deploy/kind/manifests/
deploy/kind/smoke.sh
```

## Results

**The metric.** `timing.totalUs`, measured by the caller's daprd from receiving the exec request
to sending the response, for `echo ok` on a new sandbox ID.

**The gate**
- It covers path (a): a new sandbox ID served from the warm pool.
- Pass means p99 under 100 ms, sequentially and at 20 new sandboxes per second per host pod,
  with at most 1% errors and at most 1% late ticks.
- Restore-served rows are excluded from the gate and reported as `nonpool`.

| Run | Where | Gate line |
|---|---|---|
| Kubernetes, the gate | kind on bare metal: AMD Ryzen 9 5900X, 60 GiB, Ubuntu 26.04, kernel 7.0.0-38; 2 host pods plus a caller pod | `GATE PASS (sequential n=482 p99=16.44ms err=0.0% nonpool=3.6%; rate20 n=3600 p99=11.36ms err=0.0% nonpool=0.0% late=0.0%)` |
| Local development | self-hosted on an Apple M4 Pro, 48 GiB, macOS 27.0.1 (loaded and swapping) | `GATE PASS (sequential n=421 p99=17.79ms err=0.0% nonpool=15.8%; rate20 n=1795 p99=16.25ms err=0.0% nonpool=0.3% late=0.0%)` |
| Kubernetes, nested | kind in a Lima VM on the same Mac | `GATE FAIL (sequential n=500 p99=886.88ms ...)`: the nested stack, see below |

**All four result sets.** Total and exec time, p50 / p99, in ms, from
`results/kind-linux/notes.md`:

| run | Mac total | Mac exec | Mac + favordynmods total | Mac + favordynmods exec | kind nested total | kind nested exec | kind bare metal total | kind bare metal exec |
|---|---|---|---|---|---|---|---|---|
| path a seq | 34.1 / 41.2 | 33.8 / 41.0 | 6.2 / 192.0 | 4.2 / 17.5 | 244.1 / 886.9 | 242.7 / 883.7 | 8.3 / 103.9 | 7.2 / 12.4 |
| path a rate20 | 35.2 / 42.9 | 34.9 / 42.1 | 3.0 / 9.4 | 2.8 / 9.2 | 20113.8 / 29706.0 | 9164.1 / 27363.0 | 7.2 / 11.4 | 6.3 / 10.6 |
| path b seq | 0.8 / 22.0 | 0.8 / 22.0 | 1.2 / 2.0 | 1.2 / 2.0 | 4.2 / 24.1 | 3.6 / 23.5 | 1.6 / 2.4 | 1.4 / 2.1 |
| path c seq | 93.1 / 111.7 | 29.3 / 38.2 | 97.4 / 127.6 | 2.8 / 4.2 | 290.1 / 538.9 | 149.9 / 323.7 | 82.0 / 92.2 | 5.6 / 6.6 |
| path d seq | 187.6 / 209.7 | 34.0 / 37.8 | 216.4 / 264.8 | 1.8 / 4.2 | 2533.3 / 3205.9 | 51.5 / 113.3 | 227.8 / 244.7 | 1.8 / 2.3 |

**The four paths**
- (a) a new ID from the warm pool;
- (b) a warm exec on an existing ID;
- (c) a new ID restored from the template snapshot;
- (d) a new ID booted without a template.

**How to read the table**
- **Sequential rows** include restore-served rows. Over POOL rows only, the sequential p99 is
  16.44 ms on bare metal and 17.79 ms on the Mac with favordynmods.
- **"path a rate20"** is one 60 s run: 20 requests/s on the Mac (one host), 40 requests/s on
  kind (two host pods).
- **Nested kind** ran paths (c) and (d) with 100 requests each.

**Notes**
- **Dapr's share is small.** On bare metal at rate 20, the median path (a) request took
  7.18 ms in total. Of that, 0.54 ms was daprd routing (including the cross-pod hop) and
  0.32 ms was activation; exec took 6.29 ms.
- **The nested run fails because of the stack, not Dapr.** Each microVM runs under KVM in a
  Lima VM under Apple's Virtualization framework. Activation stayed at 0.4 ms p50, but the
  first exec in a freshly restored VM took 242.7 ms p50.
- **The warm-pool refill is the next bottleneck.** At 40 new sandboxes per second per pod,
  19.7% fell back to a restore (p99 208.6 ms). At 80 and 160 per pod, 48.6% and 75.5% of
  requests got 429.
- **The favordynmods `setup`.** Every component in the measurements remounts the guest's
  cgroup2 with `favordynmods`, which works around a guest-kernel delay of about 28 ms after
  idle (see `results/idle-cliff/notes.md`).

## Privileged pods

**The driver container is privileged.** On Kubernetes, the microVM driver container
(`microvm`, in the `sandbox-pool` deployment) runs with `privileged: true`.
- It needs this to open `/dev/kvm`: an unprivileged container's device cgroup denies the
  device even when it is mounted, and kind has no KVM device plugin.
- Only that container is privileged. daprd and the caller app keep the injector's restricted
  defaults.
- An unprivileged KVM device plugin route is future work.

**Other requirements of the deployment**
- `dapr.io/max-body-size: 16Mi`, so that a capped exec fits: two output streams of up to
  4194304 bytes each.
- The kubelet setting `singleProcessOOMKill: true`, so that one OOM kill does not take the
  driver and every VM in the pod.
- A postStart hook that holds daprd until the driver's socket exists. It does not work with
  `dapr.io/enable-native-sidecar`.

## Known limitations

- **Lost sandboxes.** A sandbox is lost when placement changes for it. `expectedActivation`
  makes the loss visible.
- **Full hosts.** A full host keeps returning 429 for a caller-chosen ID. Sidecar-chosen IDs
  retry on a fresh ID.
- **The template.** It holds a warm disk and page cache only.
- **Security.** Any authenticated app in the namespace can exec in or delete any sandbox of a
  host app. There is no per-sandbox owner binding, and `accessControl` does not cover actor
  calls.
- **Body limit.** An exec response over daprd's body limit is reported as 429.

## Issue reference

Introduced by a dapr/dapr issue (draft in `proposal/issue.md`, not filed yet).

## Checklist

* [x] Code compiles correctly
* [x] Created/updated tests
* [x] Unit tests passing (the packages listed above; the full suite was not run)
* [ ] End-to-end tests passing (no e2e tests yet; measured on kind with the companion repository)
* [ ] Extended the documentation / Created issue in the https://github.com/dapr/docs/ repo
* [ ] Specification has been updated / Created issue in the https://github.com/dapr/docs/ repo
* [ ] Provided sample for the feature / Created issue in the https://github.com/dapr/docs/ repo

🤖 Generated with [Claude Code](https://claude.com/claude-code)
