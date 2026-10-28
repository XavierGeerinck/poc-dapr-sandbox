# Mac notes

## Task 11: microsandbox backend (2026-10-28)

**Setup**

| | |
|---|---|
| Host | Apple M4 Pro (Mac16,7), 14 cores, 48 GiB, macOS 27.0.1. Other work was running: load average 9–16. |
| Toolchain | rustc 1.94.0, microsandbox crate 0.7.11 (`msb 0.7.11`, `libkrunfw.5.dylib`), Hypervisor.framework |
| VMs | `python:3.12-slim`, the driver defaults: 1 vCPU and 512 MiB |
| Pool VMs | Copy-on-write full restores of one template snapshot, with no network |
| Microsandbox home | `~/.local/state/dsbm/msb` |

The microsandbox home falls back from `~/.local/state/dapr-sandbox-microvm/msb` because that path is too long for microsandbox's Unix sockets. The limit is 51 bytes on macOS.

### Release selftest

Command: `dapr-sandbox-microvm --selftest python:3.12-slim`. The image was already cached.

| Step | Run 1 | Run 2 | Cold home (image pull) |
|---|---:|---:|---:|
| Backend init | 7.5 ms | 2.6 ms | 19.5 ms |
| Template prepare | 663 ms | 633 ms | 3572 ms |
| — boot | 190 ms | 163 ms | 3047 ms |
| — snapshot | 406 ms | 403 ms | 459 ms |
| — destroy template VM | 64 ms | 64 ms | 65 ms |
| Restore (spawn one VM) | 114 ms | 114 ms | 122 ms |
| First exec `echo ok` | 36 ms | 33 ms | 30 ms |
| `exec echo ok` ×20, p50 | **0.80 ms** | 0.94 ms | 0.89 ms |
| `exec echo ok` ×20, p90 | 0.96 ms | 1.33 ms | 1.08 ms |
| Destroy | 60 ms | 61 ms | 61 ms |
| Total | 965 ms | 931 ms | 3898 ms |

Each template snapshot is 55–57 MiB on disk for a 512 MiB VM.

### Live tests

Command: `cargo test -- --ignored`, debug build, 8 of 8 passed.

| Measurement | Result |
|---|---|
| 8 concurrent restores | p50 290 ms, p90 306 ms, wall 307 ms. Earlier runs: p50 263 and 307 ms. |
| Single restore | 132 ms |
| `exec echo ok` ×20 (debug build) | p50 1.09 ms, p90 1.40 ms |
| Session `python3 -u -i -q` | Turn 1 (`x = 42`) 88 ms, turn 2 (`print(x * 2)` gives `84`) 0.5 ms |
| `timeout_ms` 500 | Returns after 502 ms with `timed_out` |
| `head -c 10485760 /dev/zero` | Returns in 90 ms with 4194304 bytes and `truncated` |
| Template prepare, no setup | 680–900 ms |
| Destroy while an exec is in flight | 66 ms |

After the run there were no `msb` processes left, and no snapshots or VMs remained in either home. Both homes held about 150 MiB of cached image layers.

## Step 5 stretch: a live, pre-imported interpreter in the template

**Outcome: it works.** A Python interpreter with modules already imported survives the template snapshot. In each restored VM it keeps running as an independent process. The test is `live_stretch_preimported_interpreter`.

**How it was set up**
1. The template's `setup` writes a small server to `/tmp/pyd.py`.
2. `setup` starts the server with `setsid python3 -u /tmp/pyd.py </dev/null >/tmp/pyd.log 2>&1 &`, then waits for its FIFOs to appear.
3. The server imports `asyncio, decimal, email.parser, http.client, json` once. It then loops: read code from the FIFO `/run/pyd.in`, run it with `exec` in a persistent globals dict, and write the captured output to `/run/pyd.out`.
4. A client is one ordinary exec:

   ```sh
   sh -c 'printf %s "$1" > /run/pyd.in && cat /run/pyd.out' sh CODE
   ```

**Why setup instead of the image entrypoint**
- The spec asks for the image entrypoint. 0.7.11 can launch one through `SandboxBuilder::background_command`/`entrypoint` (agentd's startup command), but I did not try that route.
- The setup exec exits before the snapshot, and the detached server is not one of agentd's jobs. microsandbox therefore accepts the full snapshot. That meets the requirement of running outside microsandbox's job mechanism.

**What was checked**
- In two VMs restored from one snapshot, `x = 1` and `x = 2` were each set through the daemon.
- `print(x * 10, 'http.client' in sys.modules)` then printed `10 True` and `20 True`. Each VM has its own state, and the imports came from the snapshot.

**Latency**

Each run takes 10 samples per path in one VM.

| Run | `python3 -c 'import asyncio, decimal, email.parser, http.client, json; print(1)'`, p50 | Daemon `print(1)`, p50 |
|---|---:|---:|
| 1 | 35.1 ms | 24.9 ms |
| 2 | 35.7 ms | 2.2 ms |
| 3 | 37.2 ms | 28.7 ms |
| 4 | 35.4 ms | 2.3 ms |
| 5 | 36.2 ms | 25.6 ms |
| Earlier run | 38.0 ms | 2.5 ms |

- The cold path is steady at 34–38 ms.
- The daemon path is **bimodal by run**: about 2.2–2.6 ms in half the runs and 25–29 ms in the others. Only p50s were recorded, so the split is known per run, not per call. Each run uses a freshly prepared template.
- The cause is unknown. It is not cross-vCPU wakeups: the guest has 1 vCPU, and pinning the daemon and the client to CPU 0 with `taskset` gave the same split in 6 more runs.
- Suspects not tested:
  - guest scheduler state captured in the snapshot (the daemon's vruntime);
  - how the FIFO hand-off interacts with the agent's I/O relay;
  - host scheduling under load.

**Conclusion**
- The mechanism is viable, and in its good mode it saves 30–35 ms of interpreter startup and imports per call.
- It needs:
  - a real session protocol, rather than one FIFO pair shared by all callers;
  - an explanation of the 25 ms mode before anyone claims the saving.
- As the spec requires, the PoC still only claims a warm disk and page cache.
