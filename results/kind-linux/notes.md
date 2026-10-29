# Kind notes: bare-metal Linux (thebeast.local)

## Task 17b: Kubernetes gate run on a real KVM host (2026-10-29)

**Result: GATE PASS.** Path (a) on kind on bare metal serves from the warm pool at p50 7.2 ms / p99 11.4 ms at 40 requests/s total (rate 20 per host pod, 2 pods) and p99 16.44 ms sequentially (POOL rows). The nested-virtualization run on the Mac (Task 17) failed the same gate; the cause there was the nested stack, not Dapr or microsandbox.

Gate line, verbatim:

`GATE PASS (sequential n=482 p99=16.44ms err=0.0% nonpool=3.6%; rate20 n=3600 p99=11.36ms err=0.0% nonpool=0.0% late=0.0%)`

The report merges files by (path, workload, mode, rate): "rate20" is `path-a-rate20.csv` (2400 rows) plus `sweep-rate20.csv` (1200 rows), hence n=3600. "Sequential n=482" is the 482 POOL rows of the 500; the other 18 were served by a restore (nonpool 3.6%) and the gate excludes them (the report prints a WARNING that 39.6% of all path (a) echo rows are non-POOL, almost all from the rate 80 and 160 runs). Including those 18 rows, the sequential p99 over all 500 rows is 103.9 ms; see the caveats.

### Topology

- Host: thebeast.local, bare metal (`systemd-detect-virt` = none), AMD Ryzen 9 5900X (12 cores, 24 threads), 60 GiB RAM, Ubuntu 26.04 LTS, kernel 7.0.0-38-generic, cgroup v2, `/dev/kvm` present. It also runs ollama, which was left alone.
- Docker 29.1.3, kind v0.29.0 (node image Kubernetes v1.33.1, containerd 2.1.1), helm v3.21.2, kubectl v1.33.1 (installed to /usr/local/bin for this run, checksum verified).
- Cluster `sandbox`: 3 nodes (1 control plane, 2 workers) as docker containers on the host; every node gets the host's `/dev/kvm`. Kubeconfig only in `<repo>/.kube/kind-sandbox.yaml` on thebeast; its `~/.kube/config` has the same md5 after `up.sh` as before.
- Deployment `sandbox-pool`: 2 replicas, anti-affinity, one per worker; each pod has the privileged `microvm` driver container plus daprd. Deployment `sandbox-caller` (app ID `sandbox-caller`) holds the bench binary; every run is `kubectl exec` into it, against its own daprd with `--app-id sandbox-pool --host-pods 2`. In rate mode `--rate` is per host pod ("rate 20" = 40 requests/s in total).

### Versions

- Repo HEAD at the start of the run: 8c0e3e8 (the `feat(kind): run natively on Linux hosts with KVM` commit comes on top).
- upstream/dapr: b0ce41dac on `feat/sandbox-building-block`, built on thebeast in a `golang:1.26.10` container (go.mod's `go` line); chart `upstream/dapr/charts/dapr` with `deploy/kind/helm-values.yaml` (`tag: dev`).
- microsandbox 0.7.11; image python:3.12-slim.

### Pool sizing (`deploy/kind/manifests/components.yaml`, unchanged)

- py-fast: warmPoolSize 8, refillConcurrency 4, maxSandboxesPerHost 32, memoryMiB 512 (per pod, so 16 warm VMs in the cluster).
- py-restore: warmPoolSize 0, maxSandboxesPerHost 8. py-boot: warmPoolSize 0, `template: none`, maxSandboxesPerHost 8, memoryMiB 512.
- All three: `setup: mount -o remount,favordynmods /sys/fs/cgroup`.

### Commands (reproduce on any Linux host with KVM)

Needs docker, kind, helm, kubectl, make and git on the host, a user that can run docker, and `/dev/kvm`. Nothing is installed by the scripts.

```
rsync -a --exclude target/ --exclude upstream/dapr/dist/ --exclude demo/.run/ --exclude .kube/ --exclude .superpowers/ --exclude /.git <repo>/ host:dsb/dapr-microvm/
# or: git clone https://github.com/XavierGeerinck/poc-dapr-sandbox ~/dsb/dapr-microvm, then clone the dapr fork's
# branch into it: git clone -b feat/sandbox-building-block https://github.com/XavierGeerinck/dapr ~/dsb/dapr-microvm/upstream/dapr
# (upstream/dapr is a separate, gitignored checkout, not a submodule; this run used its commit b0ce41dac)
cd ~/dsb/dapr-microvm
deploy/kind/up.sh                                  # 35 s: kind cluster, kubeconfig in .kube/kind-sandbox.yaml
deploy/kind/build-images.sh                        # dapr 147 s (Go build in a container, docker-build, kind load), driver 141 s, bench 12 s
export KUBECONFIG=$PWD/.kube/kind-sandbox.yaml
kubectl --context kind-sandbox apply -f deploy/kind/kvm-check.yaml          # selftest
helm install dapr upstream/dapr/charts/dapr -n dapr-system --create-namespace --kube-context kind-sandbox -f deploy/kind/helm-values.yaml --wait
kubectl --context kind-sandbox apply -f deploy/kind/manifests/
deploy/kind/smoke.sh                               # PASS
# the matrix: results/kind-linux/matrix.log shows every command; in short, in the caller pod
# (kubectl exec deploy/sandbox-caller -c bench), common flags --app-id sandbox-pool --host-pods 2 --dapr-url http://localhost:3500,
# 10 s pause before each sequential run, 60 s pause before each rate run:
sandbox-bench run --path a --component py-fast --count 500 --out /work/raw/path-a-seq.csv
sandbox-bench run --path b --component py-fast --count 500 --out /work/raw/path-b-seq.csv
sandbox-bench run --path c --component py-restore --count 500 --out /work/raw/path-c-seq.csv
sandbox-bench run --path d --component py-boot --count 500 --out /work/raw/path-d-seq.csv
sandbox-bench run --path a --component py-fast --workload python --count 200 --out /work/raw/path-a-python-seq.csv
sandbox-bench run --path a --component py-fast --workload session-import --count 200 --out /work/raw/path-a-session-import-seq.csv
sandbox-bench run --path a --component py-fast --mode rate --rate 20 --duration 60 --out /work/raw/path-a-rate20.csv
sandbox-bench sweep --component py-fast --rates N --duration 30 --out-dir /work/raw        # once per N in 10 20 40 80 160
kubectl cp -c bench default/<caller>:/work/raw results/kind-linux/raw
sandbox-bench report --in results/kind-linux/raw --out results/kind-linux                   # on the Mac
```

### KVM check (`kvm-check.yaml`, selftest, first run on the cluster)

backend init 24.5 ms; template prepare 3165.9 ms (boot 2762.0, snapshot 342.4, destroy 60.5; snapshot 62 MiB); restore (spawn) 130.1 ms; first exec 21.6 ms; exec echo ok x20 p50 1.28 ms, p90 1.43 ms, max 1.71 ms; destroy 60.0 ms; total 3458.8 ms; `selftest: ok`. (The restore here is the first one after the image was loaded; in the matrix path (c) restore is 75.8 ms p50 activate.)

### Host state

| When | `uptime` load | free memory (`free -m`: free / buff-cache MiB) |
|---|---|---|
| 14:26:13 before the matrix (image builds and smoke done) | 0.99 4.37 3.26 | 30254 / 27943 of 62156 |
| 14:40:18 after the last run (rate 160) | 15.78 7.25 4.48 | 28334 / 28732 |

Load before the first run was low (the 1-minute figure is 0.99; I did not inspect other processes). The load of 15.8 after the run is the tail of the overloaded rate 80 and 160 runs (restores). Memory is mostly page cache from the image builds; "available" was 57558 MiB before and 56405 MiB after.

### Breakdown (ms, p50 / p99, nearest-rank; status-200 rows only for timings; error % counts non-200 rows of all rows; late % = late ticks / expected)

Sweep rows are per host pod (2 pods). This table is computed from the raw CSVs per file (not merged like the report's).

| run | n | status 200 | total p50/p99 | host p50/p99 | activate p50/p99 | exec p50/p99 | client p50/p99 | non-POOL % (200 rows) | error % | late % |
|---|---|---|---|---|---|---|---|---|---|---|
| a seq | 500 | 500 | 8.3 / 103.9 | 7.7 / 103.3 | 0.4 / 96.7 | 7.2 / 12.4 | 8.7 / 104.3 | 3.6 | 0.0 | - |
| path-b-seq | 500 | 500 | 1.6 / 2.4 | 1.4 / 2.1 | 0.0 / 0.0 | 1.4 / 2.1 | 1.8 / 2.5 | - | 0.0 | - |
| path-c-seq | 500 | 500 | 82.0 / 92.2 | 81.5 / 91.7 | 75.8 / 86.4 | 5.6 / 6.6 | 82.3 / 92.4 | - | 0.0 | - |
| path-d-seq | 500 | 500 | 227.8 / 244.7 | 227.4 / 244.2 | 225.6 / 242.4 | 1.8 / 2.3 | 228.1 / 245.0 | - | 0.0 | - |
| path-a-python-seq | 200 | 200 | 35.9 / 39.5 | 35.3 / 39.0 | 0.3 / 0.4 | 35.0 / 38.7 | 36.2 / 39.8 | 0.0 | 0.0 | - |
| path-a-session-import-seq | 200 | 200 | 66.2 / 72.3 | 65.8 / 71.8 | 0.0 / 0.0 | 65.8 / 71.8 | 66.5 / 72.6 | - | 0.0 | - |
| path-a-rate20 | 2400 | 2400 | 7.2 / 11.4 | 6.6 / 10.8 | 0.3 / 0.5 | 6.3 / 10.6 | 7.5 / 11.7 | 0.0 | 0.0 | 0.0 |
| sweep-rate10 | 600 | 600 | 6.6 / 7.9 | 6.1 / 7.2 | 0.3 / 0.4 | 5.7 / 6.9 | 6.9 / 8.3 | 0.0 | 0.0 | 0.0 |
| sweep-rate20 | 1200 | 1200 | 7.2 / 11.3 | 6.6 / 10.8 | 0.3 / 0.5 | 6.3 / 10.5 | 7.5 / 11.6 | 0.0 | 0.0 | 0.0 |
| sweep-rate40 | 2400 | 2400 | 10.7 / 208.6 | 10.0 / 207.7 | 0.6 / 199.4 | 7.9 / 15.8 | 11.2 / 209.1 | 19.7 | 0.0 | 0.0 |
| sweep-rate80 | 4800 | 2466 | 822.8 / 1033.4 | 821.9 / 1032.9 | 812.5 / 1022.4 | 8.5 / 19.0 | 823.1 / 1033.6 | 87.8 | 48.6 | 0.1 |
| sweep-rate160 | 9600 | 2349 | 902.4 / 1147.9 | 901.7 / 1147.5 | 891.9 / 1136.7 | 8.8 / 20.2 | 902.7 / 1148.2 | 88.1 | 75.5 | 12.2 |

Rate-mode non-200 rows: all HTTP 429 (host full / pool exhausted), 9585 rows, only at rates 80 and 160. No 500s, no status 0, no timeouts.

Pool state before each rate run (pods Ready, driver acquires from the pool so far; the driver logs no per-second pool-depth line, so these counts are the available record): both `sandbox-pool` pods `2/2 Running` with 0 restarts before every run; "path=Pool" acquires per pod before path-a-rate20: 436 / 457, before sweep 10: 1627 / 1666, 20: 1928 / 1965, 40: 2506 / 2587, 80: 3498 / 3523, 160: 3649 / 3674 (matrix.log).

### Comparison (total and exec, p50 / p99, ms)

Mac baseline is `results/mac` (Task 14), Mac+favordynmods is `results/mac-favordynmods`, kind nested is `results/kind` (Task 17: Lima VM on the Mac, nested virtualization, 100 runs for paths c/d), kind bare metal is this run. Path (a) rate20 is the single `path-a-rate20.csv` run for the kind columns (Mac: one host pod, 20/s total; kind: 2 pods, 40/s total).

| run | Mac total | Mac exec | Mac+favordynmods total | Mac+favordynmods exec | kind nested total | kind nested exec | kind bare metal total | kind bare metal exec |
|---|---|---|---|---|---|---|---|---|
| path a seq | 34.1 / 41.2 | 33.8 / 41.0 | 6.2 / 192.0 | 4.2 / 17.5 | 244.1 / 886.9 | 242.7 / 883.7 | 8.3 / 103.9 | 7.2 / 12.4 |
| path a rate20 | 35.2 / 42.9 | 34.9 / 42.1 | 3.0 / 9.4 | 2.8 / 9.2 | 20113.8 / 29706.0 | 9164.1 / 27363.0 | 7.2 / 11.4 | 6.3 / 10.6 |
| path b seq | 0.8 / 22.0 | 0.8 / 22.0 | 1.2 / 2.0 | 1.2 / 2.0 | 4.2 / 24.1 | 3.6 / 23.5 | 1.6 / 2.4 | 1.4 / 2.1 |
| path c seq | 93.1 / 111.7 | 29.3 / 38.2 | 97.4 / 127.6 | 2.8 / 4.2 | 290.1 / 538.9 | 149.9 / 323.7 | 82.0 / 92.2 | 5.6 / 6.6 |
| path d seq | 187.6 / 209.7 | 34.0 / 37.8 | 216.4 / 264.8 | 1.8 / 4.2 | 2533.3 / 3205.9 | 51.5 / 113.3 | 227.8 / 244.7 | 1.8 / 2.3 |

The bare-metal "path a seq" row is over all 500 rows (482 POOL, 18 RESTORE); over the 482 POOL rows only (the gate's basis) the p99 is 16.44 ms.

### Three facts

1. Path (a) passes on bare metal: sequential p99 16.44 ms over 482 POOL rows (0 errors), and rate 20 per pod (40/s total, 3600 rows merged) p99 11.36 ms with 0 errors, 100% POOL and 0 late ticks. Compared with the nested run on the Mac (p50 244.1 / p99 886.9 ms), exec p50 fell from 242.7 ms to 7.2 ms, and activation stayed under 1 ms (0.4 ms p50).
2. The sustainable rate is bounded by the warm pool's refill, not by Dapr: at 10 and 20 per pod every request is POOL and all totals stay under 12 ms p99; at 40 per pod 19.7% of requests fall back to RESTORE (p99 208.6 ms); at 80 and 160 per pod the pool is exhausted, 48.6% and 75.5% are HTTP 429 and the 200s are 87-88% RESTORE with p50 about 0.82-0.90 s. The highest swept rate with at least 99% POOL and at most 1% errors is 20 per pod (40/s total).
3. Cold paths on bare metal: path (c) restore 82.0 / 92.2 ms total (activation 75.8 ms p50), path (d) cold boot 227.8 / 244.7 ms total (activation 225.6 ms), both with exec under 6 ms p50. Workloads: python 35.9 / 39.5 ms, session-import 66.2 / 72.3 ms (all POOL, no errors).

### Observations and caveats

- Does the gate pass? Yes, as the report defines it: sequential and rate20 p99 under 100 ms, errors under 1%, and the gate's POOL-only basis. But 18 of the 500 sequential path (a) requests (3.6%) were served by a restore (p99 over all 500 rows is 103.9 ms, restore rows 92.9-133.3 ms). These occurred because the pool was freshly filled (warm pool size 8 per pod, refill concurrency 4) and the sequential client (about 8 ms per request, two pods) consumes warm VMs faster than the pool restores them (a restore takes about 76 ms of activation, 4 at a time per pod, from the logs and path (c)); this is my reading, not separately measured: the restore rows are spread through the run, from request 10 to request 484. This is the same warm-pool refill limit as fact 2 and not a Dapr cost. A pool size or refill concurrency tune was out of scope and not tried.
- The sweep points are independent: each was preceded by a 60 s pause and the pool pods stayed Ready with 0 restarts. Nonetheless the rate 80 and 160 runs ended with a host load of 15.8 (restores), and runs are one sample each, no repeats.
- On Linux the dapr images are built in a `golang:1.26.10` container as the invoking uid/gid with its Go cache in `~/.cache/dsb-go` on thebeast, and tagged `localhost/dapr/<binary>:dev` (also `dev-linux-amd64`); the Mac Lima cluster still has only `dev-linux-arm64` images, so `helm-values.yaml` (now `tag: dev`) needs a rebuild there (`deploy/kind/build-images.sh dapr`) before a new install on the Mac.
- The kvm-check pod `kvm-check` (Completed) and the cluster are left running on thebeast.
