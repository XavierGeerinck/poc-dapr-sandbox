//! The VM layer: [`VmBackend`] and the in-memory [`fake::FakeBackend`].
//!
//! # Contract for implementations
//!
//! - **Names.** The pool picks every VM name (see [`crate::pool::vm_name`]);
//!   names match `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`. A backend must use the
//!   name as given and must not derive names from sandbox IDs.
//! - **Concurrency.** Every method may run concurrently with any other,
//!   including several `spawn`s (up to `refillConcurrency` refills plus inline
//!   spawns), `exec`s on different VMs and on the same VM, and a `destroy` of
//!   a VM that has an `exec` in flight. No pool lock is held across a call.
//! - **Cancellation.** The gRPC layer may drop an `exec`/`session_*` future
//!   when the caller goes away, and the pool drops a warm-up `exec` at its
//!   timeout or on shutdown; those must be cancel-safe. The pool never
//!   drops `spawn`, `prepare_template`, `destroy` or `list` futures.
//! - **Errors.** [`BackendError::NotFound`] is reserved for an unknown VM or
//!   session on `exec`, `session_*` and `destroy` (daprd turns it into
//!   "sandbox lost" on exec), including a VM destroyed while an `exec` on it
//!   was running. [`BackendError::InvalidArgument`] for bad input;
//!   [`BackendError::Internal`] for everything else. Failures of `spawn`,
//!   `prepare_template` and `list` reach daprd as `INTERNAL` whatever their
//!   variant (the service maps them), since daprd would read `NOT_FOUND` on
//!   Acquire as "invalid request".
//! - **Output.** At most [`OUTPUT_CAP`] bytes per stream; drop the rest and
//!   set `truncated`. The service enforces the cap again.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::Config;

/// Per-stream output cap for exec and for each session send (4 MiB).
pub const OUTPUT_CAP: usize = 4 * 1024 * 1024;

/// A backend error. Maps to a gRPC status in the service.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// Unknown VM or session. Maps to `NOT_FOUND`.
    #[error("not found: {0}")]
    NotFound(String),
    /// Bad input, such as an empty command. Maps to `INVALID_ARGUMENT`.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// Any other failure. Maps to `INTERNAL`.
    #[error("{0}")]
    Internal(String),
}

pub type Result<T, E = BackendError> = std::result::Result<T, E>;

/// A command to run in a VM; mirrors `SandboxExecSpec`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Working directory in the guest; empty means the backend's default.
    pub cwd: String,
    pub stdin: Vec<u8>,
    /// 0 means no timeout.
    pub timeout_ms: u32,
}

/// The outcome of a command or a session send; mirrors `SandboxExecResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Non-zero when the command was killed by its timeout.
    pub exit_code: i32,
    /// Some output was dropped by the [`OUTPUT_CAP`].
    pub truncated: bool,
    /// The timeout killed the command (exec) or `until` never appeared
    /// (session send).
    pub timed_out: bool,
    /// Time the command ran; 0 lets the service fill in its own measurement.
    pub elapsed_us: u64,
}

impl ExecResult {
    /// Truncates each stream to `cap` bytes, setting `truncated` if anything
    /// was dropped.
    pub fn enforce_cap(&mut self, cap: usize) {
        for stream in [&mut self.stdout, &mut self.stderr] {
            if stream.len() > cap {
                stream.truncate(cap);
                self.truncated = true;
            }
        }
    }
}

/// The VM layer the pool and service drive.
#[async_trait]
pub trait VmBackend: Send + Sync + 'static {
    /// Prepares the template `name` for `cfg` (for example: boot a VM, run
    /// `cfg.setup`, snapshot it, destroy the VM). Replaces an existing
    /// template of the same name. Called once per pool start, before any
    /// `spawn(.., Some(name))`.
    async fn prepare_template(&self, cfg: &Config, name: &str) -> Result<()>;

    /// Creates and starts VM `name`, from template `from_template` (restore)
    /// or from `cfg.image` (boot, then run `cfg.setup`). Returns once the VM
    /// can run commands.
    async fn spawn(&self, cfg: &Config, name: &str, from_template: Option<&str>) -> Result<()>;

    /// Runs a command to completion. `spec.timeout_ms > 0` kills the command
    /// after that long and returns a non-zero exit with `timed_out = true`.
    async fn exec(&self, name: &str, spec: ExecSpec) -> Result<ExecResult>;

    /// Starts a long-running command with piped stdin and returns its
    /// session ID (unique per VM).
    async fn session_start(&self, name: &str, spec: ExecSpec) -> Result<String>;

    /// Writes `input` to the session's stdin and returns the output produced
    /// since this call started, once `until` appears in stdout or stderr, the
    /// process exits, or `timeout` elapses (`timed_out = true` if `until` was
    /// set). With an empty `until` it waits `timeout` (zero: returns right
    /// after writing). The output cap applies per send; output produced
    /// between sends is discarded. `exit_code` is set once the process exited.
    async fn session_send(
        &self,
        name: &str,
        session: &str,
        input: &[u8],
        until: &[u8],
        timeout: Duration,
    ) -> Result<ExecResult>;

    /// Kills the session's process and forgets the session.
    async fn session_close(&self, name: &str, session: &str) -> Result<()>;

    /// Stops and deletes VM `name` and its sessions. `NotFound` if unknown.
    async fn destroy(&self, name: &str) -> Result<()>;

    /// Names of the existing VMs (not templates or snapshots) that start
    /// with `prefix`, including ones left over from an earlier process.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// An in-memory backend for tests and for daprd smoke runs without a
/// hypervisor. It runs no VMs and provides no isolation.
pub mod fake {
    use std::collections::{BTreeSet, HashSet};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use tokio::sync::Semaphore;

    use super::{BackendError, ExecResult, ExecSpec, Result, VmBackend};
    use crate::config::Config;

    /// Replaces the built-in command emulation of [`FakeBackend::exec`].
    pub type ExecHandler = Arc<dyn Fn(&str, &ExecSpec) -> Result<ExecResult> + Send + Sync>;

    #[derive(Default)]
    struct Inner {
        vms: BTreeSet<String>,
        templates: BTreeSet<String>,
        prepared: Vec<String>,
        spawned: Vec<(String, Option<String>)>,
        spawns_entered: usize,
        /// `None` until `record_execs`: the binary serves this backend too,
        /// and an exec log would grow without bound there.
        execs: Option<Vec<(String, ExecSpec)>>,
        destroyed: Vec<String>,
        sessions: HashSet<(String, String)>,
        session_seq: u64,

        spawn_delay: Duration,
        destroy_delay: Duration,
        prepare_delay: Duration,
        exec_delay: Duration,
        fail_spawns: u32,
        spawn_error: Option<BackendError>,
        fail_prepares: u32,
        fail_list: bool,
        spawn_gate: Option<Arc<Semaphore>>,
        exec_gate: Option<Arc<Semaphore>>,
        exec_handler: Option<ExecHandler>,
    }

    /// In-memory [`VmBackend`] with injectable delays and failures.
    ///
    /// Without an exec handler, `exec` emulates `echo`, `true`, `false`,
    /// `cat` (stdin to stdout) and `sleep <secs>` (honours `timeout_ms`);
    /// anything else exits 127. Once [`record_execs`](Self::record_execs)
    /// is called, every exec is recorded. Sessions echo their input back.
    #[derive(Default)]
    pub struct FakeBackend {
        inner: Mutex<Inner>,
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self::default()
        }

        fn lock(&self) -> MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Adds an existing VM, such as a leftover from an earlier process.
        pub fn add_vm(&self, name: impl Into<String>) {
            self.lock().vms.insert(name.into());
        }

        pub fn set_spawn_delay(&self, d: Duration) {
            self.lock().spawn_delay = d;
        }

        pub fn set_destroy_delay(&self, d: Duration) {
            self.lock().destroy_delay = d;
        }

        pub fn set_prepare_delay(&self, d: Duration) {
            self.lock().prepare_delay = d;
        }

        pub fn set_exec_delay(&self, d: Duration) {
            self.lock().exec_delay = d;
        }

        /// The next `n` spawns fail with `Internal`.
        pub fn fail_next_spawns(&self, n: u32) {
            self.fail_next_spawns_with(n, BackendError::Internal("injected spawn failure".into()));
        }

        /// The next `n` spawns fail with `err`.
        pub fn fail_next_spawns_with(&self, n: u32, err: BackendError) {
            let mut st = self.lock();
            st.fail_spawns = n;
            st.spawn_error = Some(err);
        }

        /// The next `n` template preparations fail with `Internal`.
        pub fn fail_next_prepares(&self, n: u32) {
            self.lock().fail_prepares = n;
        }

        /// While set, `list` fails with `Internal`.
        pub fn set_fail_list(&self, fail: bool) {
            self.lock().fail_list = fail;
        }

        pub fn set_exec_handler(&self, handler: ExecHandler) {
            self.lock().exec_handler = Some(handler);
        }

        /// From now on every spawn blocks until [`allow_spawns`] lets it
        /// through (or [`resume_spawns`] lifts the gate).
        ///
        /// [`allow_spawns`]: Self::allow_spawns
        /// [`resume_spawns`]: Self::resume_spawns
        pub fn pause_spawns(&self) {
            let mut st = self.lock();
            if st.spawn_gate.is_none() {
                st.spawn_gate = Some(Arc::new(Semaphore::new(0)));
            }
        }

        /// Lets `n` paused spawns through.
        pub fn allow_spawns(&self, n: usize) {
            if let Some(gate) = &self.lock().spawn_gate {
                gate.add_permits(n);
            }
        }

        /// Lifts the gate: blocked and future spawns proceed.
        pub fn resume_spawns(&self) {
            if let Some(gate) = self.lock().spawn_gate.take() {
                gate.close();
            }
        }

        /// From now on every exec blocks, after its VM check, until
        /// [`allow_execs`] lets it through (or [`resume_execs`] lifts the
        /// gate).
        ///
        /// [`allow_execs`]: Self::allow_execs
        /// [`resume_execs`]: Self::resume_execs
        pub fn pause_execs(&self) {
            let mut st = self.lock();
            if st.exec_gate.is_none() {
                st.exec_gate = Some(Arc::new(Semaphore::new(0)));
            }
        }

        /// Lets `n` paused execs through.
        pub fn allow_execs(&self, n: usize) {
            if let Some(gate) = &self.lock().exec_gate {
                gate.add_permits(n);
            }
        }

        /// Lifts the gate: blocked and future execs proceed.
        pub fn resume_execs(&self) {
            if let Some(gate) = self.lock().exec_gate.take() {
                gate.close();
            }
        }

        /// From now on, records every `exec` (see [`execs`](Self::execs)).
        pub fn record_execs(&self) {
            self.lock().execs.get_or_insert_with(Vec::new);
        }

        /// Every `exec` on an existing VM since
        /// [`record_execs`](Self::record_execs), in order, recorded on entry:
        /// including ones still blocked by
        /// [`pause_execs`](Self::pause_execs) and ones that fail.
        ///
        /// # Panics
        ///
        /// If `record_execs` was never called, so that a test cannot read an
        /// empty log as "no execs".
        pub fn execs(&self) -> Vec<(String, ExecSpec)> {
            self.lock()
                .execs
                .clone()
                .expect("FakeBackend::record_execs was not called")
        }

        /// The specs of the execs on VM `name` so far (see
        /// [`execs`](Self::execs), which also says when it panics).
        pub fn execs_on(&self, name: &str) -> Vec<ExecSpec> {
            self.execs()
                .into_iter()
                .filter(|(vm, _)| vm == name)
                .map(|(_, spec)| spec)
                .collect()
        }

        /// Existing VMs, sorted.
        pub fn vms(&self) -> Vec<String> {
            self.lock().vms.iter().cloned().collect()
        }

        /// Every successful spawn, in order: (name, from_template).
        pub fn spawned(&self) -> Vec<(String, Option<String>)> {
            self.lock().spawned.clone()
        }

        /// Calls of `spawn` so far, counted on entry: including ones still
        /// blocked by [`pause_spawns`](Self::pause_spawns) and ones that
        /// failed.
        pub fn spawns_entered(&self) -> usize {
            self.lock().spawns_entered
        }

        /// Every successful destroy, in order.
        pub fn destroyed(&self) -> Vec<String> {
            self.lock().destroyed.clone()
        }

        /// Every successful template preparation, in order.
        pub fn prepared(&self) -> Vec<String> {
            self.lock().prepared.clone()
        }

        fn has_vm(&self, name: &str) -> Result<()> {
            if self.lock().vms.contains(name) {
                Ok(())
            } else {
                Err(BackendError::NotFound(format!("unknown VM {name:?}")))
            }
        }
    }

    fn emulate(spec: &ExecSpec) -> ExecResult {
        let line = |s: String| format!("{s}\n").into_bytes();
        match spec.command.as_str() {
            "echo" => ExecResult {
                stdout: line(spec.args.join(" ")),
                ..Default::default()
            },
            "true" => ExecResult::default(),
            "false" => ExecResult {
                exit_code: 1,
                ..Default::default()
            },
            "cat" => ExecResult {
                stdout: spec.stdin.clone(),
                ..Default::default()
            },
            cmd => ExecResult {
                stderr: line(format!("fake: {cmd}: command not found")),
                exit_code: 127,
                ..Default::default()
            },
        }
    }

    #[async_trait]
    impl VmBackend for FakeBackend {
        async fn prepare_template(&self, _cfg: &Config, name: &str) -> Result<()> {
            let delay = self.lock().prepare_delay;
            tokio::time::sleep(delay).await;
            let mut st = self.lock();
            if st.fail_prepares > 0 {
                st.fail_prepares -= 1;
                return Err(BackendError::Internal("injected prepare failure".into()));
            }
            st.templates.insert(name.to_string());
            st.prepared.push(name.to_string());
            Ok(())
        }

        async fn spawn(
            &self,
            _cfg: &Config,
            name: &str,
            from_template: Option<&str>,
        ) -> Result<()> {
            let gate = {
                let mut st = self.lock();
                st.spawns_entered += 1;
                st.spawn_gate.clone()
            };
            if let Some(gate) = gate {
                // A closed gate (resume_spawns) lets everyone through.
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
            }
            let delay = self.lock().spawn_delay;
            tokio::time::sleep(delay).await;

            let mut st = self.lock();
            if st.fail_spawns > 0 {
                st.fail_spawns -= 1;
                return Err(st
                    .spawn_error
                    .clone()
                    .unwrap_or_else(|| BackendError::Internal("injected spawn failure".into())));
            }
            if let Some(t) = from_template
                && !st.templates.contains(t)
            {
                return Err(BackendError::Internal(format!("unknown template {t:?}")));
            }
            if !st.vms.insert(name.to_string()) {
                return Err(BackendError::Internal(format!(
                    "VM {name:?} already exists"
                )));
            }
            st.spawned
                .push((name.to_string(), from_template.map(str::to_string)));
            Ok(())
        }

        async fn exec(&self, name: &str, spec: ExecSpec) -> Result<ExecResult> {
            let gate = {
                let mut st = self.lock();
                if !st.vms.contains(name) {
                    return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
                }
                if let Some(log) = &mut st.execs {
                    log.push((name.to_string(), spec.clone()));
                }
                st.exec_gate.clone()
            };
            if let Some(gate) = gate {
                // A closed gate (resume_execs) lets everyone through.
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
            }
            let start = Instant::now();
            let (delay, handler) = {
                let st = self.lock();
                (st.exec_delay, st.exec_handler.clone())
            };
            tokio::time::sleep(delay).await;

            let mut res = match handler {
                Some(h) => h(name, &spec)?,
                None if spec.command == "sleep" => {
                    let secs: f64 =
                        spec.args
                            .first()
                            .and_then(|a| a.parse().ok())
                            .ok_or_else(|| {
                                BackendError::InvalidArgument("sleep needs seconds".into())
                            })?;
                    let want = Duration::from_secs_f64(secs);
                    let limit = Duration::from_millis(spec.timeout_ms.into());
                    if spec.timeout_ms > 0 && limit < want {
                        tokio::time::sleep(limit).await;
                        ExecResult {
                            exit_code: -1,
                            timed_out: true,
                            ..Default::default()
                        }
                    } else {
                        tokio::time::sleep(want).await;
                        ExecResult::default()
                    }
                }
                None => emulate(&spec),
            };
            // A VM destroyed while the command ran is gone.
            self.has_vm(name)?;
            if res.elapsed_us == 0 {
                res.elapsed_us = start.elapsed().as_micros().try_into().unwrap_or(u64::MAX);
            }
            Ok(res)
        }

        async fn session_start(&self, name: &str, spec: ExecSpec) -> Result<String> {
            if spec.command.is_empty() {
                return Err(BackendError::InvalidArgument("command is required".into()));
            }
            let mut st = self.lock();
            if !st.vms.contains(name) {
                return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
            }
            st.session_seq += 1;
            let id = format!("s{}", st.session_seq);
            st.sessions.insert((name.to_string(), id.clone()));
            Ok(id)
        }

        async fn session_send(
            &self,
            name: &str,
            session: &str,
            input: &[u8],
            until: &[u8],
            timeout: Duration,
        ) -> Result<ExecResult> {
            {
                let st = self.lock();
                if !st.vms.contains(name) {
                    return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
                }
                if !st
                    .sessions
                    .contains(&(name.to_string(), session.to_string()))
                {
                    return Err(BackendError::NotFound(format!(
                        "unknown session {session:?}"
                    )));
                }
            }
            let start = Instant::now();
            let found = !until.is_empty() && input.windows(until.len()).any(|w| w == until);
            let mut timed_out = false;
            if !found {
                tokio::time::sleep(timeout).await;
                timed_out = !until.is_empty();
            }
            Ok(ExecResult {
                stdout: input.to_vec(),
                timed_out,
                elapsed_us: start.elapsed().as_micros().try_into().unwrap_or(u64::MAX),
                ..Default::default()
            })
        }

        async fn session_close(&self, name: &str, session: &str) -> Result<()> {
            let mut st = self.lock();
            if !st.vms.contains(name) {
                return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
            }
            if st.sessions.remove(&(name.to_string(), session.to_string())) {
                Ok(())
            } else {
                Err(BackendError::NotFound(format!(
                    "unknown session {session:?}"
                )))
            }
        }

        async fn destroy(&self, name: &str) -> Result<()> {
            let delay = self.lock().destroy_delay;
            tokio::time::sleep(delay).await;
            let mut st = self.lock();
            if !st.vms.remove(name) {
                return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
            }
            st.sessions.retain(|(vm, _)| vm != name);
            st.destroyed.push(name.to_string());
            Ok(())
        }

        async fn list(&self, prefix: &str) -> Result<Vec<String>> {
            let st = self.lock();
            if st.fail_list {
                return Err(BackendError::Internal("injected list failure".into()));
            }
            Ok(st
                .vms
                .iter()
                .filter(|n| n.starts_with(prefix))
                .cloned()
                .collect())
        }
    }
}
