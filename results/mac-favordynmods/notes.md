# macOS latency with favordynmods (Task 14b)

Machine: Mac16,7, Apple M4 Pro, 48 GiB RAM, 14 CPUs. macOS 27.0.1.

Versions: repo HEAD 82fc4cb (components carry the favordynmods `setup`); upstream/dapr b0ce41dac (Task 14 used b83c44a32); microsandbox 0.7.11; image python:3.12-slim; same bench binary built from bench/. The Lima VM was stopped (`limactl stop sandbox`) for the runs.

Host state: before `limactl stop`: vm.swapusage used 7816.38M of 9216.00M, load averages 10.25 8.36 7.94. After stop: used 7808.38M, load 10.00 8.44 7.97. After the runs: used 7727.56M, load 10.89 8.70 7.99. The Mac was swapping and loaded throughout.

Component change: each of py-fast, py-restore, py-boot has `setup: mount -o remount,favordynmods /sys/fs/cgroup`. Check: `cat /proc/mounts` in a sandbox of each component shows `cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,favordynmods 0 0`. (py-boot has template none: the driver runs setup in each VM.)

Commands (each run preceded by sleep 10; `B=bench/target/release/sandbox-bench`, `R=results/mac-favordynmods/raw`):
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
$B report --in results/mac-favordynmods/raw --out results/mac-favordynmods
demo/run-selfhosted.sh down     # ended with: no leftover VMs
```

Gate line: `GATE PASS (sequential n=421 p99=17.79ms err=0.0% nonpool=15.8%; rate20 n=1795 p99=16.25ms err=0.0% nonpool=0.3% late=0.0%)`. The report also printed: WARNING: 47.5% of path (a) echo rows (2605 of 5484) have acquire_path != POOL; they are excluded from the gate.

Breakdown (ms, p50 / p99, nearest-rank, status-200 rows only for timings; error % counts non-200 rows of all rows; non-POOL % is of 200 rows):

| run | n | status 200 | total p50/p99 | host p50/p99 | activate p50/p99 | exec p50/p99 | client p50/p99 | non-POOL % (200 rows) | error % |
|---|---|---|---|---|---|---|---|---|---|
| a seq | 500 | 500 | 6.2 / 192.0 | 6.1 / 192.0 | 0.1 / 186.9 | 4.2 / 17.5 | 6.5 / 192.4 | 15.8 | 0.0 |
| b seq | 500 | 500 | 1.2 / 2.0 | 1.2 / 2.0 | 0.0 / 0.0 | 1.2 / 2.0 | 1.4 / 2.2 | n/a | 0.0 |
| c seq | 500 | 500 | 97.4 / 127.6 | 97.4 / 127.6 | 94.5 / 123.5 | 2.8 / 4.2 | 97.7 / 127.9 | 100.0 | 0.0 |
| d seq | 500 | 500 | 216.4 / 264.8 | 216.3 / 264.7 | 214.5 / 263.0 | 1.8 / 4.2 | 216.6 / 265.2 | 100.0 | 0.0 |
| a python seq | 200 | 200 | 17.5 / 175.1 | 17.5 / 175.1 | 0.1 / 158.9 | 16.6 / 24.8 | 17.9 / 175.5 | 9.0 | 0.0 |
| a session-import seq | 200 | 200 | 53.4 / 64.2 | 53.3 / 64.1 | 0.0 / 0.0 | 53.3 / 64.1 | 53.7 / 64.5 | n/a | 0.0 |
| a rate20 (60s) | 1200 | 1200 | 3.0 / 9.4 | 3.0 / 9.4 | 0.2 / 0.5 | 2.8 / 9.2 | 3.4 / 9.8 | 0.0 | 0.0 |
| sweep 10/s | 300 | 300 | 3.0 / 6.4 | 3.0 / 6.4 | 0.2 / 0.5 | 2.8 / 5.3 | 3.4 / 7.1 | 0.0 | 0.0 |
| sweep 20/s | 600 | 600 | 3.1 / 35.5 | 3.0 / 35.4 | 0.2 / 2.2 | 2.8 / 25.0 | 3.4 / 39.3 | 0.8 | 0.0 |
| sweep 40/s | 1200 | 964 | 1029.4 / 1349.7 | 1029.3 / 1349.4 | 1024.5 / 1346.4 | 4.4 / 29.4 | 1029.7 / 1350.4 | 85.9 | 19.7 |
| sweep 80/s | 2400 | 995 | 1071.2 / 1314.8 | 1071.2 / 1314.7 | 1066.0 / 1298.9 | 5.1 / 27.2 | 1071.6 / 1315.4 | 87.9 | 58.5 |
| sweep 160/s | 4800 | 925 | 1109.4 / 2225.2 | 1109.3 / 2225.2 | 1103.5 / 2208.8 | 4.5 / 26.9 | 1109.7 / 2225.6 | 88.4 | 80.7 |

Notes: in path a seq, 79 of 500 rows are RESTORE (the pool was empty); those rows carry the activate p99 of 186.9 ms. In sweep 40/80/160 the non-200 rows are status 429.

Comparison, baseline (results/mac) against favordynmods (ms, p50 / p99):

| run | baseline total p50/p99 | favordynmods total p50/p99 | baseline exec p50/p99 | favordynmods exec p50/p99 |
|---|---|---|---|---|
| a seq | 34.1 / 41.2 | 6.2 / 192.0 | 33.8 / 41.0 | 4.2 / 17.5 |
| a rate20 (60s) | 35.2 / 42.9 | 3.0 / 9.4 | 34.9 / 42.1 | 2.8 / 9.2 |
| b seq | 0.8 / 22.0 | 1.2 / 2.0 | 0.8 / 22.0 | 1.2 / 2.0 |
| c seq | 93.1 / 111.7 | 97.4 / 127.6 | 29.3 / 38.2 | 2.8 / 4.2 |
| d seq | 187.6 / 209.7 | 216.4 / 264.8 | 34.0 / 37.8 | 1.8 / 4.2 |

Facts:
1. Path a p99 total (sequential, n=500): baseline 41.2 ms, now 192.0 ms. (Rate-20 60 s run: 42.9 ms, now 9.4 ms.)
2. Path a exec p50 (sequential): baseline 33.8 ms, now 4.2 ms. (Rate-20: 34.9 ms, now 2.8 ms.)
3. Highest sweep rate with path a at least 99% POOL and at most 1% errors: baseline 20/s, now 20/s (20/s: 0.8% non-POOL, 0.0% errors; 40/s: 85.9% non-POOL, 19.7% errors).
