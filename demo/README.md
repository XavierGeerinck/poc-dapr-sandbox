# Self-hosted sandbox demo (macOS)

This demo runs Dapr's alpha Sandbox building block on one Mac. It starts one daprd and one
`dapr-sandbox-microvm` driver, which serves microsandbox microVMs through Apple's
Hypervisor.framework. You drive it over daprd's HTTP API.

```sh
demo/run-selfhosted.sh up     # build, start, wait until py-fast serves
demo/smoke.sh                 # exec, Python session, stale activation, delete
demo/run-selfhosted.sh down   # stop everything; prints "no leftover VMs"
```

## Prerequisites

- **Hardware:** Apple Silicon. The driver needs Hypervisor.framework (HVF).
- **Dapr source:** `upstream/dapr` checked out on branch `feat/sandbox-building-block`.
- **Go:** the version in `upstream/dapr/go.mod` (1.26.10).
- **Rust:** 1.94 or later.
- **protoc:** on `PATH`, or set `PROTOC`.
- **Other tools:** `curl`, `jq`, `lsof` and `shasum`, which macOS includes.

`up` rebuilds every binary each time. Go and cargo caches keep a rebuild to a few seconds.
- **From `upstream/dapr`:** daprd, placement and scheduler, built with `-tags=allcomponents`.
- **From `driver/`:** the driver, built with `cargo build --release`.

On the first run, the driver pulls `python:3.12-slim` and builds a template snapshot for each
component. That can take tens of seconds. `up` waits up to `DEMO_READY_TIMEOUT` seconds (300 by
default).

## What runs

| Process | Listens on (all 127.0.0.1) |
|---|---|
| daprd (`--app-id sandbox-host`) | HTTP API 3500, gRPC API 50101, internal gRPC 50102, metrics 50103, profiling 50104 (pinned, but profiling is off) |
| placement | gRPC 50105, healthz 50107, metrics 50108, raft 50109 |
| scheduler | gRPC 50106, healthz 50110, metrics 50111, etcd client 50112, etcd peer 50113 |
| driver | Unix socket `/tmp/dsb-demo/sockets/microvm.sock` |

- **No `dapr init`.** The demo deliberately leaves a `dapr init` installation alone: it never touches its containers on 50005/50006 or `~/.dapr`. It runs its own placement and scheduler on 50105/50106, with TLS disabled, built from the same source as daprd.
- **Ports.** `up` fails at once if any of the ports above is in use.
- **Loopback only.** Everything listens on loopback, because the sandbox API runs arbitrary commands.

**`--max-body-size 16Mi`.** daprd's default limit of 4 MiB applies to its HTTP and gRPC servers,
and also to the pluggable component connection. The driver caps each exec output stream at 4 MiB,
and a response carries two of them. A capped exec (2 × 4 MiB plus framing) only fits under the
raised limit. Over the limit, a client would get a 429 instead of `truncated: true`.

**Socket folder.** daprd connects to every socket in `DAPR_COMPONENTS_SOCKETS_FOLDER`.
- **Component type mapping:** it maps the component type `sandbox.microvm` to `microvm.sock`.
- **Why a demo-only folder:** macOS limits a Unix socket path to 104 bytes. So the demo uses a short folder of its own, `/tmp/dsb-demo/sockets`, rather than the shared default `/tmp/dapr-components-sockets`.
- **One driver per daprd:** a second daprd on the same socket would share the driver's pools.

**Components** (`demo/components/`). All three use the `python:3.12-slim` image.

| Component | Settings | Activation path (`acquirePath`) |
|---|---|---|
| `py-fast` | warm pool of 8, refilled 4 at a time | `POOL`: a running VM restored from a snapshot |
| `py-restore` | no pool | `RESTORE`: a snapshot restore on demand |
| `py-boot` | no pool, `template: none` | `BOOT`: a cold boot on demand |

`actorstore` is the in-memory actor state store; sandboxes are Dapr actors.

`up` waits only for `py-fast`. Right after `up`, the other two components may answer 429 for a
moment.

## Where things live

| What | Where | Removed by `down` |
|---|---|---|
| Binaries | `demo/.run/bin/` (daprd, placement, scheduler), `driver/target/release/` | no |
| Logs | `demo/.run/logs/<process>.log` (truncated by each `up`) | no |
| PIDs | `demo/.pids` | yes |
| Driver state: each component's properties, so a restarted driver restores its pools | `demo/.run/state/` | yes |
| Scheduler data: etcd and identity dir | `demo/.run/scheduler/` | yes |
| microsandbox home: images, snapshots, VM state | the driver's default (see below) | no |

- **Driver state.** `down` deletes the state directory. Otherwise, the next driver start would bring back the demo's warm pools.
- **microsandbox home.** The default is `~/.local/state/dapr-sandbox-microvm/msb`, or `~/.local/state/dsbm/msb` when that path is too long. On macOS microsandbox limits its home to 51 bytes.
  - To use another short path, set `DAPR_SANDBOX_MSB_HOME`.
  - `down` keeps the image cache.
- **microsandbox runtime.** The `msb` and `libkrunfw` pair lives in `driver/target/msb-runtime/`.

**What `down` does:**
1. Sends SIGTERM to daprd, the driver, the scheduler and placement, in that order. Each gets 30 s before SIGKILL. `down` only signals PIDs from `demo/.pids` whose command is still the demo's binary.
2. Checks that no `msb machine` process of the demo's components is left. These carry the VM name prefix `dsb-<sha256(component)[:8]>-`.
   - None left: prints `no leftover VMs`.
   - Some left: kills them and exits 1.

## The API by hand

The base URL is `http://127.0.0.1:3500/v1.0-alpha1/sandboxes`. Responses are protojson:
- **bytes** fields (`stdout`, `stderr`, `stdin`, `input`, `until`) are base64.
- **uint64** fields (`timing.totalUs`, `hostUs`, `activateUs`, `execUs`) are JSON strings.
- **Zero values** are omitted. A missing `exitCode` means 0.

```sh
API=http://127.0.0.1:3500/v1.0-alpha1/sandboxes

# Create: daprd picks the ID.
curl -s -X POST "$API/py-fast" -d '{}'
# {"sandboxId":"5d0e…","activation":"…","timing":{"totalUs":"402",…,"acquirePath":"POOL"}}

# Exec. The first call on a new ID activates it from the warm pool.
curl -s -X POST "$API/py-fast/demo-1/exec" \
  -d '{"command":"python3","args":["-c","print(6 * 7)"]}' |
  jq '{stdout: (.stdout // "" | @base64d), exitCode: (.exitCode // 0),
       activation, totalMs: ((.timing.totalUs | tonumber) / 1000),
       acquirePath: .timing.acquirePath}'

# Exec with stdin, env, cwd and a timeout. The timeout kills the command
# and sets timedOut.
curl -s -X POST "$API/py-fast/demo-1/exec" -d "$(jq -nc '{command: "sh",
  args: ["-c", "cat; echo $GREETING; pwd"], env: {GREETING: "hi"}, cwd: "/tmp",
  stdin: ("from stdin\n" | @base64), timeoutMs: 5000}')" | jq -r '.stdout | @base64d'

# Session: a long-lived process. Each send writes input, then waits until
# stdout or stderr contains `until`, the process exits, or timeoutMs passes.
# Python prints its prompt on stderr and results on stdout.
SID=$(curl -s -X POST "$API/py-fast/demo-1/sessions" \
  -d '{"command":"python3","args":["-u","-i","-q"]}' | jq -r .sessionId)
curl -s -X POST "$API/py-fast/demo-1/sessions/$SID" \
  -d "$(jq -nc '{input: ("x = 42\n" | @base64), until: (">>> " | @base64), timeoutMs: 5000}')"
curl -s -X POST "$API/py-fast/demo-1/sessions/$SID" \
  -d "$(jq -nc '{input: ("print(x * 2)\n" | @base64), until: ("84\n" | @base64), timeoutMs: 5000}')" |
  jq -r '.stdout | @base64d'                                  # 84
curl -s -X DELETE "$API/py-fast/demo-1/sessions/$SID"         # {}

# expectedActivation: pass the activation you saw. If the sandbox has since
# been lost or recreated, the call fails with 410 instead of running in a
# fresh VM.
curl -s -X POST "$API/py-fast/demo-1/exec" \
  -d '{"command":"true","expectedActivation":"not-the-current-one"}'
# {"errorCode":"ERR_SANDBOX_LOST","message":"sandbox was lost"}

# Delete. This releases the VM, and the pool refills.
curl -s -X DELETE "$API/py-fast/demo-1"                       # {}
```

Every route also takes `"appId"` in the body, or a `dapr-app-id` header, to reach a sandbox
hosted by another app. A sandbox left idle for 5 minutes is released.

| HTTP | `errorCode` | Meaning |
|---|---|---|
| 400 | `ERR_SANDBOX_NOT_FOUND` | no such sandbox component |
| 400 | `ERR_SANDBOX_INVALID_REQUEST` | bad request, e.g. a missing command or an unknown session |
| 410 | `ERR_SANDBOX_LOST` | the sandbox was lost, or `expectedActivation` is stale |
| 429 | `ERR_SANDBOX_HOST_FULL` | at capacity, or the component is not ready yet: retry later |
| 500 | `ERR_SANDBOX_EXEC` | the driver failed |
