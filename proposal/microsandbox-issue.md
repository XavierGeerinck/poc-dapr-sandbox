# Draft issue for superradcompany/microsandbox

A local draft for review. It has not been posted anywhere.

- **Evidence:** [`results/idle-cliff/notes.md`](../results/idle-cliff/notes.md), with the root cause and experiments E0–E4. They ran on an Apple M4 Pro with macOS 27.0.1 and Hypervisor.framework.
- **Dapr runs with the workaround:** `results/mac-favordynmods/` and `results/kind-linux/`.

---

**Title: Every exec after ≥ ~25 ms idle pays one RCU grace period (~28 ms on aarch64) because agentd writes `cgroup.procs` per exec**

**Version:** microsandbox 0.7.11, libkrunfw @ 70ff1e82 (Linux 6.12.113, aarch64 `CONFIG_HZ=100`).

**Symptom**
- `echo ok` through `Sandbox::exec` takes about 1.5 ms back-to-back.
- It takes about 28 ms whenever the VM was idle for more than about 20–25 ms. The plateau is flat from 30 ms to 2000 ms of idle.
- The first exec after a restore took 46.1 and 79.2 ms in two control runs.

**Cause**
- `ExecSession::spawn_pipe` and `spawn_pty` call `WorkloadPlacement::place_current()` in the child, i.e. `write("0")` to `/sys/fs/cgroup/microsandbox-workload/cgroup.procs`.
- In the kernel: `cgroup_procs_write_start` → `cgroup_attach_lock(true)` → `percpu_down_write(&cgroup_threadgroup_rwsem)` → `rcu_sync_enter()`.
- Once the `rcu_sync` is back to `GP_IDLE` (one grace period after the previous writer), `rcu_sync_enter` runs `synchronize_rcu()`. That takes about 28 ms in a 1-vCPU HZ=100 guest.
- Measured inside the guest: the `cgroup.procs` write costs 0.03 ms below the idle threshold and 27–29 ms above it.

**Proposed fix**
- Mount cgroup2 with `favordynmods` in agentd (`crates/agentd/lib/init.rs:190-196`), and retry with no option on `EINVAL`, because the option only exists on kernels ≥ 6.0:
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
- Alternatives:
  - boot with `cgroup_favordynmods=1`;
  - set `CONFIG_CGROUP_FAVOR_DYNMODS=y` in libkrunfw;
  - spawn exec children with `clone3(CLONE_INTO_CGROUP)`, which never write-locks the rwsem.

**Workaround, verified**
- Remount at runtime: `mount -o remount,favordynmods /sys/fs/cgroup`.
- Run it in the template VM before the snapshot, and restored VMs inherit it.
- Exec p50 after ≥ 20 ms of idle went from 28.1–31.6 ms to 2.2–3.5 ms. The host was loaded during this run.
- The first exec after a restore went from 46.1 / 79.2 ms to 3.9 / 3.7 ms.
- In a Dapr sandbox benchmark on the same Mac, the exec time of a new sandbox's first command (p50) went from 33.8 ms to 4.2 ms.

**Scope:** the cause is in the guest kernel and agentd, so it is not specific to one hypervisor and applies to KVM as well as HVF. The x86_64 libkrunfw has `HZ=250`, so its cliff should be shorter. That is an estimate; it was not measured.
