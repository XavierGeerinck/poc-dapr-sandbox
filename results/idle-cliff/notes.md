# Investigation: the ~25 ms idle cliff in exec latency (microsandbox 0.7.11, HVF)

Date: 2026-10-29. Machine: Apple M4 Pro, macOS 27.0.1. Investigation only: nothing in the repo was changed except this report.

## TL;DR

- **Root cause (high confidence).** The delay is inside the guest kernel. It is not in HVF, libkrun or the host relay.
  - For every exec, the forked child runs agentd's `place_current()`, which writes `"0"` to `/sys/fs/cgroup/microsandbox-workload/cgroup.procs`.
  - In Linux 6.12, a `cgroup.procs` write takes `cgroup_threadgroup_rwsem` for writing: `percpu_down_write()`, then `rcu_sync_enter()`.
  - When that rwsem's `rcu_sync` has gone back to idle, `rcu_sync_enter()` calls `synchronize_rcu()`. That is one full normal RCU grace period, which takes about 25–30 ms in this guest. The libkrunfw aarch64 kernel has `CONFIG_HZ=100`, NO_HZ, and 1 vCPU.
- **The idle threshold has the same source.** After a writer leaves, `rcu_sync` stays "hot" for one more grace period (`GP_EXIT`, then `GP_IDLE`).
  - A writer that arrives within about 20–25 ms of the previous exec's write takes the fast path.
  - A later writer pays a full grace period, of about 28 ms. That is the flat plateau.
- **Best fix, verified, no code change.** Use the driver's existing template `setup` property to run one guest knob before the snapshot. Restored VMs inherit it through the memory snapshot. Either knob works:
  - `mount -o remount,favordynmods /sys/fs/cgroup` (preferred: narrow), or
  - `echo 1 > /sys/kernel/rcu_expedited`.

  The table below is p50 over 20 samples each. The host was loaded during this run (load average 30–64 from unrelated processes).

  | | Control | `favordynmods` | `rcu_expedited` |
  |---|---:|---:|---:|
  | Exec p50, idle ≥ 20 ms | 28.1–31.6 ms | 2.2–3.5 ms | 2.1–3.8 ms |
  | First exec after a restore | 46 / 79 ms | 3.7 / 3.9 ms | 3.9 / 10.8 ms |

- **Not HVF-specific.** The cause is the guest kernel plus agentd, so Linux/KVM shows it too.
  - The aarch64 libkrunfw on a Linux/KVM host has the same `HZ=100`, so expect the same ~25–30 ms.
  - The x86_64 libkrunfw has `HZ=250`, so expect a shorter cliff (my estimate).
  - Nested KVM probably makes the grace period much longer. That is likely the 260–380 ms seen in kind on Lima, but I did not verify it.

## Sources and versions

Everything is under `<scratch>/src/idle-cliff/`, a scratch directory outside this repo (not committed).

| Component | Version | Where |
|---|---|---|
| microsandbox (SDK, runtime `msb`, agentd) | tag `v0.7.11`, commit `df3e0f7dd692` | `microsandbox/` (git clone). The registry crates are identical; I diffed `runner/console.rs` and `runner/vm.rs`. |
| libkrun fork "msb_krun" | `msb_krun*` **0.1.41**, pinned `=0.1.41` in `microsandbox/Cargo.toml:99` and in its `Cargo.lock` | `msb_krun-0.1.41/` (copied from the crates.io registry). Repo `zerocore-ai/libkrun` has no matching tag. |
| libkrunfw | `superradcompany/libkrunfw` @ `70ff1e821a7b` (submodule pin at v0.7.11). Kernel **Linux 6.12.113**, ABI 5 (`libkrunfw.5.dylib`). | `libkrunfw/` |
| Linux | 6.12.113: the files used below only | `linux-6.12.113/` (from `gregkh/linux` tag `v6.12.113`) |

The prebuilt runtime in `driver/target/msb-runtime/bin/msb` reports `msb 0.7.11`. It is ad-hoc signed, without hardened runtime, and has the `com.apple.security.hypervisor` and `disable-library-validation` entitlements. I did not rebuild it.

The paths below are relative to the trees above. msb_krun paths are written as `<crate>-0.1.41/...`.

## Exec path, end to end

**Host SDK to the msb process**

1. The driver calls `MicrosandboxBackend::exec` (`driver/src/microsandbox_backend.rs:992`), then `run` (`:732`), then `Sandbox::exec_stream_with` (`:707`).
2. The SDK path: `microsandbox` `lib/sandbox/mod.rs:1243` → `lib/sandbox/exec/session.rs:643`.
3. The agent client connects to the VM's agent Unix socket: `microsandbox-agent-client` `lib/optimized.rs:273` (`connect`).
4. This step was measured at about 1 ms in every case (task 15a instrumentation).

**msb relay to the libkrun virtio-console port**

5. The relay listens on the Unix socket (`crates/runtime/lib/runner/relay.rs:1195`), runs from `Relay::run` (`:1966`), and frames host-to-guest messages.
6. It pushes each frame into `ConsoleSharedState.rx_ring` and calls `rx_wake.wake()`, a 1-byte pipe write (`relay.rs:2524-2527`, `push_guest_frame_until`).
7. Its guest-to-host reader awaits `tx_wake.notified()` (`relay.rs:4255`, `:4343`). This path is event-driven.
8. Its only timed waits (`tx_wake.wait_timeout`, up to 10 ms, at `relay.rs:1588` and `:1922`) are in the pre-ready and restore-thaw handshakes, not in steady state.
9. The console backend is `AgentConsoleBackend` (`crates/runtime/lib/runner/console.rs`): `read` at `:418` drains `rx_wake`, `write` at `:461` pushes to `tx_ring` and wakes `tx_wake` (`:474`), and `read_wake_fd` is at `:480`.

**libkrun virtio-console device and interrupt injection (msb_krun_devices 0.1.41)**

10. The RX thread blocks in `ConsolePortBackendInputAdapter::wait_until_readable`. That is `poll(wake_fd, stopfd, PollTimeout::NONE)` (`src/virtio/console/port_io.rs:814-823`), with no timeout.
11. `process_rx` (`src/virtio/console/process_rx.rs:11-76`) copies the data into the guest's descriptors, then calls `interrupt.signal_used_queue()` (`:61`, `:67`), then `MmioTransport::try_signal_used_queue` (`src/virtio/mmio.rs:178`), then the irqchip.
12. On macOS 15+ the in-kernel GIC is used (`msb_krun_vmm-0.1.41/src/builder.rs:1754`). Its `HvfGicV3::set_irq` calls `hv_gic_set_spi(irq, true)` (`src/legacy/hvfgicv3.rs:130-136`).
13. TX direction: the guest's QueueNotify MMIO write leads to the queue eventfd, the console event handler (`event_handler.rs:28-37`), `notify_tx` (thread unpark, `port.rs:129`), `process_tx` (`process_tx.rs:10`), then `AgentConsoleBackend::write`.
14. The VMM event loop is `event_manager.run()` (`msb_krun-0.1.41/src/api/vm.rs:705`), which calls `epoll.wait(-1)` (`msb_krun_polly-0.1.41/src/event_manager.rs:206`). It has no timeout.

**vCPU**

15. The HVF run loop is `Vcpu::run` (`msb_krun_vmm-0.1.41/src/macos/vstate.rs:786`) and `HvfVcpu::run` (`msb_krun_hvf-0.1.41/src/lib.rs`).
16. A user-space WFI path exists: `EC_WFX_TRAP` (`lib.rs:1277`) leads to `wait_for_event` (`vstate.rs:949`), a crossbeam select with the guest timer deadline. It is woken by `VcpuList::set_irq_common` (`legacy/vcpu.rs:29-48`).
17. On this host that path is **never taken** (see experiment E1). macOS 27 with `hv_gic_create` handles WFI inside `hv_vcpu_run`.

**Guest: agentd (microsandbox v0.7.11, `crates/agentd`)**

18. The main loop is `run` (`lib/agent.rs:478`), which uses `tokio::select!` on an `AsyncFd` over the virtio-console port (`:648`). It is epoll-driven.
19. On `MessageType::ExecRequest` (`agent.rs:2839`), agentd calls `workload.placement()` (`:2880`), then `ExecSession::spawn` (`:2896`, `lib/session.rs:689`), then `spawn_pipe` (`session.rs:1257`). The PTY path is `spawn_pty` (`:1003`, `:1104`).
20. Inside `Command::pre_exec`, the forked child calls `placement.place_current()` (`session.rs:1296`). That is `write(fd, "0")` on `/sys/fs/cgroup/microsandbox-workload/cgroup.procs` (`lib/workload.rs:13`, `:287-311`).
21. `Command::spawn()` returns only after the child has exec'd, through the CLOEXEC status pipe. Only then does agentd send `ExecStarted` (`agent.rs:2907`).
22. So the guest's `ExecStarted` waits for the cgroup write.
23. agentd mounts cgroup2 with no options (`lib/init.rs:188-196`).

**Guest kernel: Linux 6.12.113 (libkrunfw config: `CONFIG_HZ=100`, `NO_HZ_FULL=y`, `TREE_RCU`, `PREEMPT_NONE`, `# CONFIG_CGROUP_FAVOR_DYNMODS is not set`)**

24. `__cgroup_procs_write` (`kernel/cgroup/cgroup.c:5244`) calls `cgroup_procs_write_start` (`:2978`).
25. That sets `*threadgroup_locked = pid || threadgroup` (`:2996`). For `cgroup.procs`, `threadgroup` is true, so the lock is taken even for pid 0.
26. `cgroup_attach_lock` (`:2490-2495`) calls `percpu_down_write(&cgroup_threadgroup_rwsem)` (`kernel/locking/percpu-rwsem.c:224-232`), which calls `rcu_sync_enter(&sem->rss)`.
27. In `rcu_sync_enter` (`kernel/rcu/sync.c:105-140`), state `GP_IDLE` means **`synchronize_rcu()`** (`:133`). Any other state means `wait_event(gp_state >= GP_PASSED)`, which returns at once in `GP_EXIT`/`GP_REPLAY`.
28. `synchronize_rcu` (`kernel/rcu/tree.c:4108-4123`) uses `synchronize_rcu_expedited()` if `rcu_gp_is_expedited()`. Otherwise it waits for a full normal grace period. On 6.12, `rcu_blocking_is_gp()` is false once the scheduler runs (`:4024-4031`).
29. On unlock, `rcu_sync_exit` (`sync.c:152-166`) sets `GP_EXIT` and calls `call_rcu_hurry(rcu_sync_func)`. One grace period later, `rcu_sync_func` (`:58-89`) sets `GP_IDLE`, unless a writer came in between.
30. The upstream escape hatch: `CGRP_ROOT_FAVOR_DYNMODS` (`include/linux/cgroup-defs.h:88-99`). Its comment says: *"Reduce latencies on dynamic cgroup modifications such as task migrations … by disabling percpu operation on cgroup_threadgroup_rwsem. This makes hot path operations such as forks and exits into the slow path and more expensive."*
    - It can be set through the cgroup2 mount option `favordynmods` (`cgroup.c:1957`), the boot parameter `cgroup_favordynmods=` (`:6953-6957`), or `CONFIG_CGROUP_FAVOR_DYNMODS`.
    - It calls `rcu_sync_enter()` once and never exits (`cgroup_favor_dynmods`, `:1304-1316`).
    - The same comment notes that seeding with `CLONE_INTO_CGROUP` does not write-lock the rwsem at all.

## Candidate wait points: ruled in and ruled out

| Candidate | Verdict | Evidence |
|---|---|---|
| Host SDK, socket and relay | Out | Connect and send took about 1 ms in every case (task 15a). The relay is event-driven (`notified().await`). Its 10 ms timed waits apply only before ready and during a restore thaw. |
| Console RX thread poll or backoff | Out | `poll(..., PollTimeout::NONE)` on the wake pipe (`port_io.rs:814-823`). No spin-then-sleep. |
| VMM event loop timeout | Out | `epoll.wait(-1)`, i.e. kqueue with no timeout. |
| Interrupt coalescing | Out | `signal_used_queue` calls `hv_gic_set_spi` directly. In the traces the first guest exit follows the SPI by 0.03–0.6 ms (E1, E4). |
| HVF WFI handling: user-space wait with no kick from `hv_gic_set_spi` | Out (on this host) | E1: 0 WFx exits and 0 `hv_vcpus_exit` calls over the whole run. The vCPU stays inside one `hv_vcpu_run` for up to 940 ms while idle. An SPI injected after 900 ms of idle still causes a guest exit within 0.04 ms. HVF handles WFI in the kernel when `hv_gic_create` is in use. See the side note below. |
| Host CPU QoS, E-cores, DVFS | Out | E4: during a slow exec, the guest stays inside one uninterrupted `hv_vcpu_run` with no exits for 13.5–26.5 ms. It is blocked, not running slowly on the host. E2 reproduces the 28 ms entirely inside the guest, with no host work in the timed region. |
| agentd poll intervals or timers | Out | The main loop is `AsyncFd`, i.e. epoll. Its timers (5 ms credit flush, 100 ms heartbeat thread) cannot produce a constant delay that starts at the request. Knobs that change only RCU or cgroup locking remove the delay completely. |
| Guest NO_HZ / HZ=100 | Contributing | Sets the grace-period length (~25–30 ms). It is not the trigger on its own. |
| **cgroup.procs write → `percpu_down_write` → `rcu_sync_enter` → `synchronize_rcu`** | **In** | E2: the write itself costs 0.03 ms below the threshold and 27–29 ms above it. `rcu_expedited=1` (which affects only `synchronize_rcu`) and `favordynmods` (which affects only this rwsem's `rcu_sync`) each remove the delay. Only that call chain lies in both. |

**Side note for upstream libkrun (not this bug).** I observed none of these on this host, so they are latent at most.
- `HvfGicV3::set_irq` (`hvfgicv3.rs:130-150`) only calls `hv_gic_set_spi`. It never wakes a vCPU parked in the user-space `wait_for_event` (`vstate.rs:949`). On an HVF version that traps WFx to user space while the in-kernel GIC is active, the vCPU would sleep until its guest timer fires.
- Also, `EC_WFX_TRAP` computes the timeout as `cval - mach_absolute_time()` (`hvf lib.rs:1277-1292`), ignoring the vtimer offset that restores set. It also uses integer `1e9 / cntfrq`.

## Why the threshold is ~20–25 ms and the plateau a flat ~28 ms

- **Plateau.** One normal RCU grace period in an idle 1-vCPU guest at HZ=100. It is measured directly in the guest at p50 27.4–29.0 ms (E2). It does not depend on how long the VM was idle, because the cost starts when the request arrives. Hence the flat line from 30 ms to 2000 ms.
- **Threshold.** After the previous exec's write, `rcu_sync_exit` queues a callback that returns the rwsem to `GP_IDLE` one grace period later, so after about 20–30 ms. Until then, the next writer takes the fast path. So the threshold is about one grace period too, which is why it roughly equals the plateau.
- **Jitter near the threshold.** At 20–22 ms of idle the samples are bimodal (about 1.5 ms or about 30 ms; E0, E3). That is jiffy alignment of the grace-period end.
- **Edge cases.**
  - A request that lands just as the exit callback completes (25 ms of idle) waits slightly longer (~31–33 ms).
  - The first exec after a restore pays the same grace period on top of the restore.
- **The earlier `sleep 0.01` guest loop did not help.** Waking the vCPU every 10 ms does not shorten a grace period, which is paced by RCU's jiffy-based kthread waits.
- **"Per VM".** The state is in each guest kernel.

## Experiments

All of them used my own home, `/tmp/dsbinv/msb`. That home and every VM were removed afterwards, and `pgrep -x msb` is empty. I ran no limactl, kind, kubectl or docker commands. The tools are under `<scratch>/inv/` (not committed):
- `harness/`: a Rust harness that drives the driver's own `MicrosandboxBackend`, so it uses the same exec path;
- `hvtrace/hvtrace.c`: an HVF interposer;
- `tools/*.py`: the analysis scripts.

The host was busy with unrelated work during E2–E4 (load average 30–64: biome, OrbStack, Virtualization.framework). The absolute numbers are noisier than in task 15a, but the separation is large.

**E0: baseline reproduction.** The repo's own `live_exec_latency_vs_idle` binary (copied, `DSB_LIVE_MSB_HOME=/tmp/dsbinv/msb`).

| Idle (ms) | 0 | 5 | 10 | 15 | 20 | 25 | 30 | 50 | 100 | 500 | 2000 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| p50 (ms) | 1.6 | 2.1 | 1.9 | 1.9 | 1.8 | 33.2 | 28.3 | 27.7 | 28.0 | 28.5 | 28.1 |

**E1: HVF trace.** I injected `hvtrace.dylib` into `msb` through `DYLD_INSERT_LIBRARIES`. It interposes `hv_vcpu_create`, `hv_vcpu_run`, `hv_vcpus_exit` and `hv_vcpu_set_vtimer_mask`, plus `dlsym("hv_gic_set_spi")`. The trace file is `<scratch>/inv/traces/trace1-restored-vm.bin`.
- **Exits.** Of 4,070 exits, all are data aborts (MMIO). There are 0 WFx exits and 0 kicks. Idle in-run intervals last up to 941 ms.
- **SPI to exit.** 0.01–0.05 ms after long idle; 0.02–4 ms while busy.
- **Anatomy of one exec after 318 ms of idle.**
  - The request SPI arrives at 0.
  - Three console round trips happen within 1.4 ms.
  - Then **22.9 ms inside one `hv_vcpu_run` with no exits**: the guest is blocked.
  - At 24.3 ms the guest kicks another virtio device (SPI 40; I did not map which device it is). The `ExecStarted`/stdout/exit frames follow at about 25–26 ms.

**E2: in-guest probe** (`tools/probe.py`, run inside the VM). The process moves itself between two cgroups by writing `"0"` to `cgroup.procs` after the given idle time. That is the same syscall agentd's child makes, with no host involvement in the timed region. Values are p50 of 6, in ms. Bracketed values are the maximum, where it differs a lot from the p50.

| Idle (ms) | 0 | 5 | 10 | 15 | 20 | 25 | 30 | 50 | 100 | 500 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Default | 0.00 | 0.03 | 0.03 [30.7] | 0.02 [20.9] | 0.07 [28.0] | 0.06 [23.4] | **28.9** | **29.0** | **27.4** | **28.7** |
| `rcu_expedited=1` (runtime) | 0.00 | 0.03 | 0.02 | 0.02 | 0.03 | 0.04 | 0.04 | 0.04 | 0.04 | 0.05 |
| `favordynmods` (runtime remount) | 0.02 | 0.04 | 0.04 | 0.04 | 0.05 | 0.05 | 0.04 | 0.03 | 0.04 | 0.03 |

The probe also timed `membarrier(MEMBARRIER_CMD_GLOBAL)`. That measurement is invalid on 1 vCPU, because the kernel short-circuits it when only one CPU is online, so I discarded it.

In the same VM, the exec sweep (p50 of 8) went from 27.7–31.4 ms at idle ≥ 25 ms to **1.3–1.7 ms at every idle** after `echo 1 > /sys/kernel/rcu_expedited`.

**E3: fix through the existing driver knob.** The component `setup` runs in the template before the snapshot, so restored VMs inherit the setting. I checked inside each restored VM: `rcu_expedited=1`, or `favordynmods` in `/proc/mounts`. I ran the order control, expedited, favordynmods twice and pooled the results: p50 of 20 per cell, in ms.

| Idle (ms) | Control | `setup: echo 1 > /sys/kernel/rcu_expedited` | `setup: mount -o remount,favordynmods /sys/fs/cgroup` |
|---:|---:|---:|---:|
| 0 | 2.4 | 3.0 | 3.3 |
| 5 | 3.5 | 3.7 | 2.5 |
| 10 | 3.8 | 4.3 | 2.8 |
| 15 | 2.8 | 4.2 | 2.9 |
| 20 | **29.7** | 3.2 | 2.7 |
| 25 | **31.6** | 3.3 | 2.7 |
| 30 | **28.7** | 3.1 | 3.5 |
| 50 | **29.0** | 3.0 | 2.2 |
| 100 | **28.1** | 2.6 | 2.3 |
| 500 | **29.5** | 2.1 | 2.4 |
| 2000 | **29.2** | 3.8 | 2.9 |
| First exec after restore (rep 1 / rep 2) | 46.1 / 79.2 | 3.9 / 10.8 | 3.9 / 3.7 |

The fixed columns sit around 2–4 ms rather than 1.5 ms because the host was loaded. In E2, on the same session's runtime toggle, the fixed values were 1.3–1.7 ms.

**E4: trace before and after, in one VM** (`traces/trace3-restored-vm.bin`, `tools/perexec.py`). I ran six execs after 300 ms of idle each, then set `rcu_expedited=1` and ran six more.

| | Exec latency (ms) | Longest exit-free `hv_vcpu_run` inside the exec (ms) | SPI to first exit (ms) |
|---|---|---|---|
| Before | 26.6–70.5 | 13.5–26.5 | 0.03–0.6 |
| After | 1.8–4.7 | 0.4–2.2 | 0.03–0.09 |

## Fix options, ranked

1. **An existing knob, no code change (verified, E3).** Set the component property `setup` to:
   - `mount -o remount,favordynmods /sys/fs/cgroup`: **preferred**. It is the kernel's documented knob for exactly this access pattern, and it scopes the change to `cgroup_threadgroup_rwsem`.
     - Side effect: every fork and exit in the guest takes the rwsem's slow path, i.e. a shared atomic instead of a per-CPU counter. That cost is in nanoseconds, and negligible at 1–2 vCPUs.
     - No idle CPU cost and no power cost.
   - Or `echo 1 > /sys/kernel/rcu_expedited`.
     - Side effect: every `synchronize_rcu()` in the guest becomes expedited.
     - On multi-vCPU guests that sends IPIs to all online vCPUs, which wakes idle vCPU threads on the host. That costs a little CPU and power, but only when something calls `synchronize_rcu`. There is no idle cost.
     - The blast radius is wider than favordynmods.
   - Both work the same on Linux/KVM.
   - Caveats:
     - In `snapshot` mode it applies through the template. In `none` mode, `setup` runs in every VM.
     - It needs `sh` and `mount` in the image, which python:3.12-slim has.
     - A user's own `setup` must chain it (`...; <user setup>`).
   - A small driver change (not made) could prepend it to every template's setup by default.
2. **Upstream microsandbox agentd: mount cgroup2 with `favordynmods`.** This is the cleanest real fix: one line, and it covers all SDK users. In `crates/agentd/lib/init.rs:190-196`:
   ```diff
            mount_ignore_busy(
                Some("cgroup2"),
                "/sys/fs/cgroup",
                Some("cgroup2"),
                nodev_noexec_nosuid,
   -            None::<&str>,
   +            Some("favordynmods"),
            )?;
   ```
   It needs a fallback retry with `None` on `EINVAL`, because the option exists only on kernels ≥ 6.0 and agentd may run on other kernels. libkrunfw 6.12 has it.

   A more structural alternative: spawn exec children with `clone3(CLONE_INTO_CGROUP)` instead of writing `cgroup.procs` in `pre_exec` (`session.rs:1104`, `:1296`). Per the kernel comment, that never write-locks the rwsem. It also closes the PID-assignment race the current code guards against. The cost is a custom spawn instead of `std::process::Command`.
3. **Upstream microsandbox-runtime: kernel cmdline.**
   - Append `cgroup_favordynmods=1` (or `rcupdate.rcu_expedited=1`) in `agent_kernel_cmdline` (`crates/runtime/lib/runner/vm.rs:3898-3907`):
     ```diff
      fn agent_kernel_cmdline(...) -> String {
          let mut cmdline = thp_kernel_cmdline(thp);
     +    cmdline.push_str(" cgroup_favordynmods=1");
     ```
   - This only takes effect at boot, so restored VMs get it from their template's boot.
   - There is no user-facing cmdline knob in 0.7.11: the cmdline is only THP plus the agent transport.
4. **libkrunfw: `CONFIG_CGROUP_FAVOR_DYNMODS=y`** in `config-libkrunfw_{aarch64,x86_64,riscv64}`. Same effect as option 3, but it needs a kernel rebuild, and it changes the behaviour for every libkrun user of that fork.
5. **Not recommended.**
   - `CONFIG_HZ=1000` shortens the grace period but does not remove it.
   - `idle=poll`-style busy idling burns a host core per vCPU and does not address the cause.
   - Host-side keep-alive execs every < 20 ms per VM would cost more than they save.

## Applicability to Linux/KVM

- **The cause is not HVF-specific.**
  - The HVF interrupt path is fast (E1, E4).
  - The delay is a guest-kernel `synchronize_rcu()` triggered by agentd's per-exec `cgroup.procs` write.
  - It happens under any VMM that runs this guest kernel and agentd.
- **aarch64 Linux/KVM hosts** use the same `HZ=100` aarch64 libkrunfw and should show the same ~25–30 ms cliff. That includes the M4 → Lima → kind nested case.
- **x86_64** libkrunfw has `HZ=250`, so the grace period, and the cliff, should be shorter (my estimate; not measured).
- **The nested KVM figure** (first exec 260–380 ms against 9.5 ms back-to-back) fits the same mechanism with a much longer grace period under nested timer virtualization. I did not verify it, because the Lima VM was off-limits.
  - A cheap check: set the same `setup` knob for the kind deployment and compare.
  - Or run `python3 probe.py` inside a pod's microVM.
- All fixes above (options 1–4) are hypervisor-independent.

## Draft upstream issue (superradcompany/microsandbox), for review

Moved to [`proposal/microsandbox-issue.md`](../../proposal/microsandbox-issue.md) (a local draft, not posted).

## Artifacts

All under `<scratch>/`, outside this repo (not committed):
- `src/idle-cliff/`: the sources;
- `inv/harness/`: the harness;
- `inv/hvtrace/`: the interposer;
- `inv/tools/`: `hvstat.py`, `spilat.py`, `episode.py`, `perexec.py`, `probe.py`, `summarize.py`;
- `inv/traces/`: the trace files;
- `inv/baseline1.txt`, `trace1.txt`, `exp1.txt`, `exp2.txt`, `exp2.csv`, `exp3.txt`, `exp3.csv`: the raw outputs.
