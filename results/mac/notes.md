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

## Task 14: self-hosted benchmark

Machine: Mac16,7, Apple M4 Pro, 51539607552 bytes RAM (48 GiB), 14 CPUs. macOS 27.0.1 (build 26A434).

Versions: repo HEAD e2032d9; upstream/dapr b83c44a32; microsandbox 0.7.11; image python:3.12-slim; rustc 1.94.0 (4a4ef493e 2026-03-02); go1.26.10 darwin/arm64 (GOTOOLCHAIN=go1.26.10).

Commands, in order (each run preceded by sleep 10; `B=bench/target/release/sandbox-bench`, `R=results/mac/raw`):
```
demo/run-selfhosted.sh up; sleep 15
$B run --path a --component py-fast --count 500 --out $R/path-a-seq.csv
$B run --path b --component py-fast --count 500 --out $R/path-b-seq.csv
$B run --path c --component py-restore --count 500 --out $R/path-c-seq.csv
$B run --path d --component py-boot --count 500 --out $R/path-d-seq.csv
$B run --path a --component py-fast --workload python --count 200 --out $R/path-a-python-seq.csv
$B run --path a --component py-fast --workload session-import --count 200 --out $R/path-a-session-import-seq.csv
$B run --path a --component py-fast --mode rate --rate 20 --duration 60 --host-pods 1 --out $R/path-a-rate20.csv
$B sweep --component py-fast --rates 10,20,40,80,160 --duration 30 --host-pods 1 --out-dir $R
$B report --in results/mac/raw --out results/mac
demo/run-selfhosted.sh down
```

(A first attempt using `cargo run` in a shell variable failed to launch, so no run happened; the runs above used the built binary.)

Gate line: `GATE PASS (sequential n=500 p99=41.25ms err=0.0% nonpool=0.0%; rate20 n=1800 p99=42.41ms err=0.0% nonpool=0.0% late=0.0%)`. The report also printed: WARNING: 46.6% of path (a) echo rows (2623 of 5627) have acquire_path != POOL; they are excluded from the gate.

Breakdown (ms, p50 / p99, nearest-rank, status-200 rows only for timings; error % counts non-200 rows of all rows; late % = late ticks / expected):

| run | n | status 200 | total p50/p99 | host p50/p99 | activate p50/p99 | exec p50/p99 | client p50/p99 | non-POOL % (200 rows) | error % | late % |
|---|---|---|---|---|---|---|---|---|---|---|
| a seq | 500 | 500 | 34.1 / 41.2 | 34.0 / 41.2 | 0.1 / 1.4 | 33.8 / 41.0 | 34.3 / 41.5 | 0.0 | 0.0 | - |
| b seq | 500 | 500 | 0.8 / 22.0 | 0.8 / 22.0 | 0.0 / 0.0 | 0.8 / 22.0 | 0.9 / 22.2 | - | 0.0 | - |
| c seq | 500 | 500 | 93.1 / 111.7 | 93.1 / 111.7 | 63.6 / 80.7 | 29.3 / 38.2 | 93.4 / 112.0 | - | 0.0 | - |
| d seq | 500 | 500 | 187.6 / 209.7 | 187.6 / 209.7 | 154.4 / 175.3 | 34.0 / 37.8 | 187.9 / 210.0 | - | 0.0 | - |
| a python seq | 200 | 200 | 42.5 / 48.8 | 42.4 / 48.7 | 0.1 / 1.2 | 42.2 / 48.7 | 42.7 / 49.0 | 0.0 | 0.0 | - |
| a session-import seq | 200 | 200 | 40.4 / 60.7 | 40.4 / 60.6 | 0.0 / 0.0 | 40.4 / 60.6 | 40.7 / 61.1 | n/a (acquire_path empty) | 0.0 | - |
| a rate20 (60s) | 1200 | 1200 | 35.2 / 42.9 | 35.2 / 42.9 | 0.1 / 4.1 | 34.9 / 42.1 | 35.5 / 43.2 | 0.0 | 0.0 | 0.0 |
| sweep 10/s | 300 | 300 | 34.7 / 41.9 | 34.6 / 41.8 | 0.1 / 0.3 | 34.5 / 41.7 | 35.0 / 42.2 | 0.0 | 0.0 | 0.0 |
| sweep 20/s | 600 | 600 | 34.8 / 41.3 | 34.8 / 41.2 | 0.1 / 3.4 | 34.6 / 41.1 | 35.1 / 41.6 | 0.0 | 0.0 | 0.0 |
| sweep 40/s | 1200 | 999 | 1005.0 / 1177.3 | 1005.0 / 1177.2 | 968.1 / 1142.4 | 35.3 / 61.2 | 1005.4 / 1177.6 | 84.7 | 16.8 | 0.0 |
| sweep 80/s | 2400 | 1001 | 1038.5 / 1549.5 | 1038.4 / 1549.4 | 1002.5 / 1516.2 | 35.3 / 70.9 | 1038.9 / 1549.7 | 87.5 | 58.3 | 0.2 |
| sweep 160/s | 4800 | 1027 | 1055.3 / 1247.0 | 1055.3 / 1247.0 | 1018.3 / 1210.6 | 34.9 / 61.3 | 1055.5 / 1247.3 | 87.7 | 78.6 | 2.0 |

Notes on the table: in session-import rows the acquire_path column is empty in the CSV, so non-POOL % is not computable for that run. In sweep 40/80/160 the non-200 rows are status 429 (no acquire_path); the 200 rows there are mostly RESTORE.

Facts:
1. Path a p99 total is 41.2 ms (sequential n=500) against 100 ms; 42.9 ms in the 60 s rate-20 run.
2. Path a exec_us p50/p99 is 33.8 / 41.0 ms; path b exec_us p50/p99 is 0.8 / 22.0 ms; difference 33.0 ms at p50 and 19.0 ms at p99. Path a exec_us is 33.8 of 34.1 ms total at p50.
3. The highest sweep rate with path a rows at least 99% POOL and at most 1% errors is 20/s (10/s and 20/s: 100% POOL, 0% errors; 40/s: 84.7% non-POOL of 200 rows, 16.8% errors).
