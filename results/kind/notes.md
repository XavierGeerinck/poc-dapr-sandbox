# Kind notes

## Task 17: Kubernetes gate run (2026-10-29)

**Result: GATE FAIL.** Path (a) on this nested-virtualization cluster is far above 100 ms sequentially, and rate mode collapses (almost every request is a 429 or an error).

Gate line, verbatim:

`GATE FAIL (sequential n=500 p99=886.88ms err=0.0% nonpool=0.0%; rate20 n=4 p99=25064.91ms err=98.9% nonpool=89.5% late=52.8%): sequential p99 886.88ms >= 100ms; rate20 p99 25064.91ms >= 100ms; rate20 err 98.9% > 1% (non-200 incl. status 0); rate20 fewer than 100 POOL rows (4); rate20 late 52.8% > 1%`

The report merges files by (path, workload, mode, rate): the "rate20" in the gate line is `path-a-rate20.csv` (2400 rows) plus `sweep-rate20.csv` (1200 rows), hence n=4 POOL rows there.

### Topology

- Mac16,7 (Apple M4 Pro, macOS 27.0.1) runs a Lima VM `sandbox` (vz) with 8 vCPU / 12 GiB, Ubuntu 24.04 with the HWE kernel 7.0.0-42-generic. KVM in the guest runs in nVHE mode under Apple's nested virtualization.
- In the VM: Docker 29.1.3 and kind v0.29.0 (Kubernetes v1.33.1, containerd 2.1.1). Cluster `sandbox` has 3 nodes: 1 control plane, 2 workers.
- Deployment `sandbox-pool`: 2 replicas, required pod anti-affinity on `kubernetes.io/hostname`, so one per worker. Each pod holds the privileged `microvm` driver container plus daprd.
- Deployment `sandbox-caller` (app ID `sandbox-caller`) holds the bench binary; every run is `kubectl exec` into it, against its own daprd on localhost:3500 with `--app-id sandbox-pool --host-pods 2`. In rate mode `--rate` is per host pod, so "rate 20" means 40 requests/s in total.

### Versions

- Repo HEAD at the start of the run: 71c7fa8 (this task adds the `py-boot` component on top).
- upstream/dapr: b0ce41dac on `feat/sandbox-building-block`. The Dapr images were rebuilt first (`deploy/kind/build-images.sh dapr`); daprd logs `Starting Dapr Runtime -- version edge -- commit b0ce41dac...`. The chart is upstream's own, `upstream/dapr/charts/dapr` (version 0.0.0), with `deploy/kind/helm-values.yaml`.
- microsandbox 0.7.11; image python:3.12-slim.

### Nested virtualization caveat

Every guest exit of a microVM in a kind pod traps through two hypervisors: the microsandbox VM runs under KVM in the Lima guest, which itself runs under Apple's Virtualization framework. Restores, first exec after restore, and anything that touches many pages cost far more than on bare metal or directly on the Mac. The numbers below are the cost of this particular stack, not of Dapr or of microsandbox on production hardware.

### Privileged pods

The `microvm` container is privileged because opening `/dev/kvm` needs it: an unprivileged container's device cgroup denies the device. daprd and the caller are not privileged. The kind kubelets set `singleProcessOOMKill: true` (`deploy/kind/kind.yaml`), so a memory-limit OOM kills one process (normally the VM that grew) and not the whole container.

### Pool sizing (`deploy/kind/manifests/components.yaml`)

- py-fast: warmPoolSize 8, refillConcurrency 4, maxSandboxesPerHost 32, memoryMiB 512 (per pod, so 16 warm VMs in the cluster).
- py-restore: warmPoolSize 0, maxSandboxesPerHost 8.
- py-boot (added in this task): warmPoolSize 0, `template: none`, maxSandboxesPerHost 8, memoryMiB 512, same favordynmods `setup` and scope as the others.
- All three: `setup: mount -o remount,favordynmods /sys/fs/cgroup`.

### Commands

```
deploy/kind/build-images.sh dapr
kubectl --context kind-sandbox apply -f deploy/kind/manifests/components.yaml   # py-boot created
kubectl rollout restart: dapr-operator, dapr-sentry, dapr-sidecar-injector, dapr-placement-server, dapr-scheduler-server, sandbox-pool, sandbox-caller
deploy/kind/smoke.sh                                                            # PASS, 10/10, split 5/5
# in the caller pod (kubectl exec deploy/sandbox-caller -c bench), each preceded by sleep 10; common flags: --app-id sandbox-pool --host-pods 2 --dapr-url http://localhost:3500
sandbox-bench run --path a --component py-fast --count 500 --out /work/raw/path-a-seq.csv
sandbox-bench run --path b --component py-fast --count 500 --out /work/raw/path-b-seq.csv
sandbox-bench run --path c --component py-restore --count 100 --out /work/raw/path-c-seq.csv
sandbox-bench run --path d --component py-boot --count 100 --out /work/raw/path-d-seq.csv
sandbox-bench run --path a --component py-fast --workload python --count 200 --out /work/raw/path-a-python-seq.csv
sandbox-bench run --path a --component py-fast --workload session-import --count 200 --out /work/raw/path-a-session-import-seq.csv
sandbox-bench run --path a --component py-fast --mode rate --rate 20 --duration 60 --out /work/raw/path-a-rate20.csv
sandbox-bench sweep --component py-fast --rates 10,20,40,80,160 --duration 30 --out-dir /work/raw
kubectl cp -c bench default/<caller>:/work/raw results/kind/raw
bench/target/release/sandbox-bench report --in results/kind/raw --out results/kind
```

No run failed outright, so none was retried. Counts for paths (c) and (d) are 100, as ruled.

### Host state

The Mac was swapping before the run.

| When | `sysctl vm.swapusage` | `uptime` (Mac) | Lima VM load |
|---|---|---|---|
| 12:06 before the image build | used 7711.56M of 9216.00M | load 4.67 6.39 7.14 | 0.55 0.51 0.22 (VM had just restarted) |
| 12:29 just before the first run | used 7487.44M of 9216.00M | load 9.75 16.11 13.62 | 3.21 5.26 2.62 |
| 12:58 after the last run | used 6978.19M of 8192.00M | load 7.20 7.64 9.40 | 75.56 54.64 27.34 |

The Mac load was inflated by the Go builds just before the run. The VM load of 75 after the run is the aftermath of the overloaded rate runs (VMs restoring and timing out). A probe 20 s later still got 200 in 0.18-0.21 s (3 of 3), so the pool recovered.

**Incident during the build.** The first `build-images.sh dapr` hit "no space left on device" on the Mac (1.6 GiB free of 926 GiB; the Go build cache was 17 GB). The VM's root filesystem went read-only (ext4 `errors=remount-ro`) and the VM would not boot (no SSH for 10+ minutes). I ran `go clean -cache`, repaired the guest filesystem from the Mac with `e2fsck -fy` (Homebrew e2fsprogs, partition offset 1074790400; it fixed bitmap and free-count differences), started the VM, and ran `deploy/kind/up.sh`. All 3 nodes came back Ready. Nothing in the VM was lost; the images were rebuilt after that. The measurements were taken after the recovery.

### Breakdown (ms, p50 / p99, nearest-rank; status-200 rows only for timings; error % counts non-200 rows of all rows; late % = late ticks / expected)

Sweep rows are per host pod (2 pods).

| run | n | status 200 | total p50/p99 | host p50/p99 | activate p50/p99 | exec p50/p99 | client p50/p99 | non-POOL % (200 rows) | error % | late % |
|---|---|---|---|---|---|---|---|---|---|---|
| a seq | 500 | 500 | 244.1 / 886.9 | 243.4 / 885.8 | 0.4 / 5.1 | 242.7 / 883.7 | 244.6 / 887.4 | 0.0 | 0.0 | - |
| b seq | 500 | 500 | 4.2 / 24.1 | 3.7 / 23.5 | 0.0 / 0.0 | 3.6 / 23.5 | 4.7 / 26.3 | - | 0.0 | - |
| c seq | 100 | 100 | 290.1 / 538.9 | 289.4 / 538.0 | 138.1 / 252.2 | 149.9 / 323.7 | 290.6 / 539.5 | - | 0.0 | - |
| d seq | 100 | 100 | 2533.3 / 3205.9 | 2532.8 / 3205.2 | 2476.2 / 3132.9 | 51.5 / 113.3 | 2533.8 / 3206.4 | - | 0.0 | - |
| a python seq | 200 | 200 | 923.6 / 8200.7 | 923.1 / 8199.7 | 0.4 / 3.0 | 922.7 / 8199.4 | 924.0 / 8201.3 | 0.0 | 0.0 | - |
| a session-import seq | 200 | 199 | 1072.3 / 4271.6 | 1071.8 / 4270.9 | 0.0 / 0.0 | 1071.8 / 4270.9 | 1072.8 / 4272.3 | n/a | 0.5 | - |
| a rate20 (60s, 2 pods) | 2400 | 21 | 20113.8 / 29706.0 | 20089.9 / 29632.6 | 10419.1 / 18047.2 | 9164.1 / 27363.0 | 20121.2 / 29756.7 | 95.2 | 99.1 | 52.3 |
| sweep 10/s/pod | 600 | 14 | 20338.5 / 29352.0 | 20334.5 / 29318.9 | 4416.9 / 16634.4 | 8544.7 / 25161.6 | 20349.0 / 29360.4 | 71.4 | 97.7 | 11.3 |
| sweep 20/s/pod | 1200 | 17 | 21448.3 / 29697.0 | 21377.3 / 29643.5 | 10329.9 / 15400.9 | 10195.4 / 27433.8 | 21455.3 / 29713.2 | 82.4 | 98.6 | 53.8 |
| sweep 40/s/pod | 2400 | 20 | 22664.3 / 28053.9 | 22223.2 / 28041.3 | 2497.7 / 15340.6 | 14215.1 / 28040.7 | 22710.6 / 28063.3 | 65.0 | 99.2 | 77.8 |
| sweep 80/s/pod | 4800 | 17 | 17121.1 / 23072.6 | 16418.0 / 22733.8 | 10920.4 / 17700.1 | 6339.3 / 20931.5 | 18084.3 / 23457.9 | 88.2 | 99.6 | 97.0 |
| sweep 160/s/pod | 9600 | 15 | 21513.8 / 28364.9 | 19205.6 / 28359.5 | 9325.2 / 19156.9 | 9455.2 / 17929.9 | 25528.1 / 29657.2 | 86.7 | 99.8 | 95.9 |

Rate-mode status mix (non-200 rows): 429 (host full: "acquire failed: host is full: 32 sandboxes", about 9300 and 10600 log lines on the two drivers), 500 (restore failures: "restored VM Generation ID acknowledgement timed out" and "restored workload thaw timed out", 375 on one driver), and status 0 (30 s client timeout). Of the few 200 rows, most are RESTORE, so the warm pool of 8 per pod was drained at once and could not refill.

### Comparison: Mac baseline vs Mac with favordynmods vs kind (total and exec p50 / p99, ms)

Mac baseline is `results/mac` (Task 14), Mac with favordynmods is `results/mac-favordynmods`, kind is this run. On the Mac, paths (c) and (d) had n=500; on kind n=100. Mac rate20 used one host pod (20/s total); kind used 2 pods (40/s total), and 99% of kind's rate20 requests failed, so its percentiles come from 21 rows.

| run | Mac total | Mac exec | Mac+favordynmods total | Mac+favordynmods exec | kind total | kind exec |
|---|---|---|---|---|---|---|
| path a seq | 34.1 / 41.2 | 33.8 / 41.0 | 6.2 / 192.0 | 4.2 / 17.5 | 244.1 / 886.9 | 242.7 / 883.7 |
| path a rate20 | 35.2 / 42.9 | 34.9 / 42.1 | 3.0 / 9.4 | 2.8 / 9.2 | 20113.8 / 29706.0 | 9164.1 / 27363.0 |
| path b seq | 0.8 / 22.0 | 0.8 / 22.0 | 1.2 / 2.0 | 1.2 / 2.0 | 4.2 / 24.1 | 3.6 / 23.5 |
| path c seq | 93.1 / 111.7 | 29.3 / 38.2 | 97.4 / 127.6 | 2.8 / 4.2 | 290.1 / 538.9 | 149.9 / 323.7 |
| path d seq | 187.6 / 209.7 | 34.0 / 37.8 | 216.4 / 264.8 | 1.8 / 4.2 | 2533.3 / 3205.9 | 51.5 / 113.3 |

### Three facts

1. Path (a) p99 total on kind is 886.9 ms sequentially (n=500, all POOL, 0 errors) against the 100 ms target. In the rate-20 run (2 pods) the p99 of the 21 successful rows is 29706.0 ms, with 99.1% errors and 52.3% late ticks.
2. Path (a) exec p50 on kind is 242.7 ms (p99 883.7 ms); path (b) exec p50 on kind is 3.6 ms (p99 23.5 ms). The first exec in a freshly restored VM costs about 239 ms more than an exec in a VM that already ran one; activation is 0.4 ms in both cases.
3. No sweep rate qualifies: at every rate (10, 20, 40, 80, 160 per pod) fewer than 1% of rows were POOL-served 200s and 97.7-99.8% were errors. The highest rate with at least 99% POOL and at most 1% errors is none. The sequential runs passed with 100% POOL and 0% errors, but there are no rate rows to qualify.

### Observations and caveats

- Sequential path (a) with 500 sequential requests at about 244 ms p50 fits the preview (about 220 ms). The favordynmods remount did not remove the first-exec cost here, as it did on the Mac.
- Workload `python` (p50 923.6 ms, p99 8200.7 ms) and `session-import` (p50 1072.3 ms, one 504) are far slower than echo, since an interpreter start in a freshly restored VM touches many cold pages through two hypervisors.
- Path (d) is 2533 ms p50, almost all activation (a cold boot); path (c) is 290 ms.
- Rate runs followed each other with a 10 s pause. After the first overloaded run, the next ones began while the Lima VM was still at high load (restores timing out), so the sweep numbers are not independent of the 60 s rate run that preceded them. Even so, nothing here shows a rate at which the pool keeps up: the pool has 8 warm VMs per pod and refills them at the restore cost, so at 10 requests/s/pod the warm pool drains in seconds.
- Not tuned, no rows dropped.
