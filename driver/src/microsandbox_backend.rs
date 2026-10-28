//! The microsandbox backend: each sandbox is a libkrun microVM (KVM on
//! Linux, Hypervisor.framework on macOS) run by the `microsandbox` crate.
//!
//! # Layout
//!
//! One microsandbox home per driver process ([`MicrosandboxBackend::new`])
//! holds VM state, the image cache, snapshots and the VMs' Unix sockets.
//! For a template `t` (the pool's [`crate::pool::template_name`]):
//!
//! 1. VM `<t>-tpl` boots from the image (replacing any VM of that name) and
//!    runs `setup` with `sh -c`;
//! 2. a full snapshot (memory, devices, disk) of it is published into the
//!    snapshot group `<t>-snap` and selected as the group's head; older
//!    members of the group are removed;
//! 3. the template VM is destroyed.
//!
//! Pool VMs are then copy-on-write restores of `<t>-snap`. Without a
//! template, each VM boots from the image and runs `setup` itself.
//!
//! # Network
//!
//! Pool VMs never have network access. Without `setup`, the template VM, and
//! so every VM restored from it, has no network device at all
//! (`disable_network`). With `setup`, which may need to download things, the
//! template VM keeps microsandbox's default network (egress allowed) while
//! `setup` runs. The snapshot captures that network device, and a full
//! restore cannot remove a captured device, so restored VMs have an `eth0`,
//! but are restored with `NetworkPolicy::none()`: microsandbox's host-side
//! network stack refuses every connection and fails every DNS lookup (its
//! DNS interceptor still accepts TCP connections to port 53 but resolves
//! nothing). VMs booted without a template use `disable_network`, so their
//! `setup` runs offline; use the snapshot template if `setup` needs the
//! network.
//!
//! # Timeouts
//!
//! Every microsandbox call that could hang has a generous but finite bound;
//! see [`timeouts`]. A call that hits its bound fails with `Internal`, and
//! the VM it was creating, if any, is destroyed.
//!
//! # Cancellation
//!
//! `exec` and `session_*` futures may be dropped at any await. A dropped
//! `exec` sends SIGKILL to its command (and microsandbox's relay kills a
//! client's sessions when its connection closes); a dropped session start
//! kills the new process; a dropped send leaves the session usable.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use microsandbox::backend::{Backend, LocalBackend, set_default_backend, with_backend};
use microsandbox::sandbox::exec::{ExecOptionsBuilder, ExecSink};
use microsandbox::sandbox::{DestroyOptions, SandboxStatus};
use microsandbox::setup::{InstallOptions, ensure_runtime};
use microsandbox::storage::MemoryPruneOptions;
use microsandbox::{
    ExecControl, ExecEvent, ExecHandle, MicrosandboxError, NetworkPolicy, Sandbox, Snapshot,
    Storage,
};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::backend::{BackendError, ExecResult, ExecSpec, OUTPUT_CAP, Result, VmBackend};
use crate::config::Config;

/// Longest microsandbox home that works: microsandbox puts each VM's
/// control socket at `<home>/run/sandboxes/<24 hex>/control.sock` (the home
/// plus 52 bytes), which must be shorter than `sun_path` (104 bytes on
/// macOS, 108 on Linux).
pub const MAX_MSB_HOME_LEN: usize = if cfg!(target_os = "linux") {
    108 - 53
} else {
    104 - 53
};

/// File name of the firmware library of microsandbox 0.7.11's runtime pair
/// (`microsandbox_utils::libkrunfw_filename`).
const LIBKRUNFW: &str = if cfg!(target_os = "macos") {
    "libkrunfw.5.dylib"
} else {
    "libkrunfw.so.5.6.1"
};

/// Bounds on microsandbox calls that could otherwise hang. Each is many
/// times the slowest run measured on an Apple-silicon Mac (Task 11 report);
/// hitting one fails the call with `Internal`.
pub mod timeouts {
    use std::time::Duration;

    /// Open the microsandbox database (measured: tens of ms).
    pub const OPEN: Duration = Duration::from_secs(60);
    /// Install (download, ~30 MB) the runtime pair when none is installed.
    pub const RUNTIME_INSTALL: Duration = Duration::from_secs(600);
    /// Boot a VM from its image, including a first pull of the image
    /// (measured: 0.2–0.4 s cached, 4.7 s with a pull of
    /// `python:3.12-slim`).
    pub const CREATE: Duration = Duration::from_secs(600);
    /// Run `setup` (template VM, or each VM without a template). Setup may
    /// install packages, so this is the most generous bound.
    pub const SETUP: Duration = Duration::from_secs(1800);
    /// Capture a full snapshot of the template VM (measured: ~0.5–0.6 s for
    /// 512 MiB).
    pub const SNAPSHOT: Duration = Duration::from_secs(180);
    /// Select a snapshot group's head, or remove one snapshot.
    pub const SNAPSHOT_ADMIN: Duration = Duration::from_secs(60);
    /// Restore a VM from a full snapshot (measured: 0.17–0.4 s).
    pub const RESTORE: Duration = Duration::from_secs(60);
    /// Destroy a VM, all in (measured: ~0.1 s).
    pub const DESTROY: Duration = Duration::from_secs(30);
    /// The stop budget within [`DESTROY`] (the stop is forced anyway).
    pub const DESTROY_STOP: Duration = Duration::from_secs(10);
    /// Open an exec stream: connect to the guest agent and start the
    /// process (measured: ~1 ms).
    pub const EXEC_START: Duration = Duration::from_secs(30);
    /// After killing a command (timeout, session close), how long to wait
    /// for its exit status.
    pub const KILL_GRACE: Duration = Duration::from_secs(5);
    /// One page of the VM list, one status probe, the snapshot list.
    pub const QUERY: Duration = Duration::from_secs(30);
}

use timeouts::*;

const FORCE_DESTROY: DestroyOptions = DestroyOptions {
    force: true,
    timeout: DESTROY_STOP,
};

/// The snapshot group of template `t`.
fn snapshot_group(template: &str) -> String {
    format!("{template}-snap")
}

/// Whether the template VM gets a network device: only to run `setup`.
fn template_network(cfg: &Config) -> bool {
    !cfg.setup.is_empty()
}

fn internal(what: impl std::fmt::Display, e: impl std::fmt::Display) -> BackendError {
    BackendError::Internal(format!("{what}: {e}"))
}

/// Any error as `Internal`: spawn and template failures reach daprd as
/// `INTERNAL` whatever their kind.
fn as_internal(e: BackendError) -> BackendError {
    match e {
        BackendError::Internal(m)
        | BackendError::InvalidArgument(m)
        | BackendError::NotFound(m) => BackendError::Internal(m),
    }
}

/// Runs `fut`, failing with `Internal` after `limit`.
async fn within<T>(
    what: impl std::fmt::Display,
    limit: Duration,
    fut: impl Future<Output = T>,
) -> Result<T> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| BackendError::Internal(format!("{what}: timed out after {limit:?}")))
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Appends what fits under [`OUTPUT_CAP`], setting `truncated` if anything
/// was dropped.
fn push_capped(buf: &mut Vec<u8>, data: &[u8], truncated: &mut bool) {
    let room = OUTPUT_CAP.saturating_sub(buf.len());
    if data.len() > room {
        *truncated = true;
    }
    buf.extend_from_slice(&data[..data.len().min(room)]);
}

/// Whether `needle` (non-empty) occurs in `hay` at or after `*from`; moves
/// `*from` to the first position a later match could start at.
fn find_from(hay: &[u8], needle: &[u8], from: &mut usize) -> bool {
    let start = (*from).min(hay.len());
    let found = hay[start..].windows(needle.len()).any(|w| w == needle);
    *from = hay.len().saturating_sub(needle.len() - 1);
    found
}

/// Bytes allocated on disk under `path` (sparse files count what they use).
fn disk_usage(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let own = md.blocks() * 512;
    if !md.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| disk_usage(&e.path()))
                .sum::<u64>()
        })
        .unwrap_or(0)
        + own
}

/// Sends SIGKILL to a command from a detached task (for use in `Drop`).
fn kill_later(control: ExecControl) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            let _ = tokio::time::timeout(KILL_GRACE, control.kill()).await;
        });
    }
}

/// Kills an exec's command when dropped while armed.
struct KillOnDrop(Option<ExecControl>);

impl KillOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            kill_later(control);
        }
    }
}

fn exec_options(
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: String,
    stdin: Option<Vec<u8>>,
) -> impl FnOnce(ExecOptionsBuilder) -> ExecOptionsBuilder {
    move |o| {
        let mut o = o.args(args).envs(env);
        if !cwd.is_empty() {
            o = o.cwd(cwd);
        }
        match stdin {
            None => o.stdin_pipe(),
            Some(data) if data.is_empty() => o.stdin_null(),
            Some(data) => o.stdin_bytes(data),
        }
    }
}

/// Timings of the last [`VmBackend::prepare_template`], for `--selftest`.
#[derive(Debug, Clone, Default)]
pub struct PrepareTimings {
    /// Booting the template VM from its image.
    pub boot: Duration,
    /// Running `setup` (zero without one).
    pub setup: Duration,
    /// Capturing and publishing the full snapshot.
    pub snapshot: Duration,
    /// Destroying the template VM.
    pub destroy: Duration,
    /// Everything, including selecting the head and pruning old snapshots.
    pub total: Duration,
    /// Size of the snapshot on disk.
    pub snapshot_bytes: Option<u64>,
}

/// A VM this process created.
struct Vm {
    sandbox: Sandbox,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Vm {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a session's process produced, filled by its event pump.
#[derive(Default)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    started: bool,
    /// The process failed to start.
    failed: Option<String>,
    exit: Option<i32>,
    /// The event stream ended: the process exited, or the VM or its agent
    /// went away.
    ended: bool,
}

#[derive(Default)]
struct SessionOutput {
    captured: Mutex<Captured>,
    changed: Notify,
}

impl SessionOutput {
    fn lock(&self) -> MutexGuard<'_, Captured> {
        self.captured.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until `ready` holds.
    async fn wait_for(&self, mut ready: impl FnMut(&Captured) -> bool) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready(&self.lock()) {
                return;
            }
            notified.await;
        }
    }
}

/// Moves a session's events into its [`SessionOutput`].
async fn pump(mut handle: ExecHandle, out: Arc<SessionOutput>) {
    while let Some(event) = handle.recv().await {
        {
            let mut guard = out.lock();
            let c = &mut *guard;
            match event {
                ExecEvent::Started { .. } => c.started = true,
                ExecEvent::Stdout(data) => push_capped(&mut c.stdout, &data, &mut c.truncated),
                ExecEvent::Stderr(data) => push_capped(&mut c.stderr, &data, &mut c.truncated),
                ExecEvent::Exited { code } => c.exit = Some(code),
                ExecEvent::Failed(f) => c.failed = Some(f.message),
                ExecEvent::StdinError(_) => {}
            }
        }
        out.changed.notify_waiters();
    }
    out.lock().ended = true;
    out.changed.notify_waiters();
}

/// A long-running command with piped stdin.
struct Session {
    stdin: ExecSink,
    control: ExecControl,
    out: Arc<SessionOutput>,
    pump: tokio::task::AbortHandle,
    /// One send at a time.
    send: tokio::sync::Mutex<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.pump.abort();
        if !self.out.lock().ended {
            kill_later(self.control.clone());
        }
    }
}

/// The [`VmBackend`] on microsandbox. See the module docs.
///
/// Only one should exist per process: [`new`](Self::new) makes it
/// microsandbox's process-wide default backend.
pub struct MicrosandboxBackend {
    local: Arc<dyn Backend>,
    home: PathBuf,
    /// Never held across an await.
    vms: Mutex<HashMap<String, Arc<Vm>>>,
    session_seq: AtomicU64,
    last_prepare: Mutex<Option<PrepareTimings>>,
}

impl MicrosandboxBackend {
    /// Opens (creating it, mode 0700) the microsandbox home `msb_home`, an
    /// absolute path of at most [`MAX_MSB_HOME_LEN`] bytes, and resolves the
    /// runtime pair (`msb`, `libkrunfw`): `MSB_PATH`/`MSB_LIBKRUNFW_PATH` if
    /// set, else the pair the build installed (see `build.rs`), else
    /// `<msb_home>/{bin,lib}`, else a download into `<msb_home>`.
    pub async fn new(msb_home: PathBuf) -> Result<Self> {
        let len = msb_home.as_os_str().len();
        if !msb_home.is_absolute() {
            return Err(BackendError::InvalidArgument(format!(
                "microsandbox home {} is not an absolute path",
                msb_home.display()
            )));
        }
        if len > MAX_MSB_HOME_LEN {
            return Err(BackendError::InvalidArgument(format!(
                "microsandbox home {} is {len} bytes; at most {MAX_MSB_HOME_LEN} leave room \
                 for the Unix socket paths microsandbox derives from it",
                msb_home.display()
            )));
        }
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&msb_home)
                .map_err(|e| internal(format!("creating {}", msb_home.display()), e))?;
        }
        register_build_runtime();

        let local = within(
            "opening the microsandbox home",
            OPEN,
            LocalBackend::builder()
                .home(&msb_home)
                .config_path(msb_home.join("config.json"))
                .build(),
        )
        .await?
        .map_err(|e| internal(format!("opening {}", msb_home.display()), e))?;
        let runtime = within(
            "resolving the microsandbox runtime",
            RUNTIME_INSTALL,
            ensure_runtime(local.config(), InstallOptions::default()),
        )
        .await?
        .map_err(|e| internal("resolving the microsandbox runtime", e))?;
        info!(home = %msb_home.display(), msb = %runtime.msb_path.display(),
            libkrunfw = %runtime.libkrunfw_path.display(), origin = ?runtime.origin,
            "microsandbox backend");

        let local: Arc<dyn Backend> = Arc::new(local);
        // Static microsandbox calls run under `with_backend`; this covers
        // any that microsandbox makes from tasks of its own.
        set_default_backend(Arc::clone(&local));
        Ok(Self {
            local,
            home: msb_home,
            vms: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            last_prepare: Mutex::new(None),
        })
    }

    /// The microsandbox home.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Timings of the last successful template preparation.
    pub fn last_prepare_timings(&self) -> Option<PrepareTimings> {
        self.last_prepare
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Deletes template `name`'s snapshots (its template VM is gone once
    /// `prepare_template` returns). Fails while VMs restored from it exist.
    /// Not part of [`VmBackend`]: pools keep their template; `--selftest`
    /// and the live tests clean up with it.
    pub async fn remove_template(&self, name: &str) -> Result<()> {
        let group = snapshot_group(name);
        let head = match self.group_head(&group).await {
            Ok(head) => Some(head),
            Err(MicrosandboxError::SnapshotNotFound(_)) => None,
            Err(e) => return Err(internal(format!("reading the head of {group}"), e)),
        };
        // Other members first: a group's head can only go last.
        self.prune(&group, head.as_deref()).await?;
        if let Some(head) = head {
            self.prune(&group, None)
                .await
                .map_err(|e| internal(format!("removing snapshot {head} of {group}"), e))?;
        }
        self.prune_memory_cache().await;
        Ok(())
    }

    /// Deletes the RAM images (`<home>/cache/memory`) that copy-on-write
    /// restores realized from snapshots and no running VM still maps.
    /// microsandbox keeps them after the snapshot itself is removed: about
    /// the guest's touched memory (~70 MB of 512 MiB for an idle Python
    /// VM) per snapshot ever restored. Best effort; logs failures.
    async fn prune_memory_cache(&self) {
        let options = MemoryPruneOptions {
            dry_run: false,
            older_than: Duration::ZERO,
            max_entries: None,
            branches_only: false,
        };
        let pruned = within(
            "pruning the memory cache",
            SNAPSHOT_ADMIN,
            self.scoped(async { Storage::prune(&options).await }),
        )
        .await;
        match pruned {
            Ok(Ok(report)) => debug!(
                files = report.files_removed,
                logical_bytes = report.logical_bytes_removed,
                "pruned the memory cache"
            ),
            Ok(Err(e)) => warn!("pruning the memory cache: {e}"),
            Err(e) => warn!("{e}"),
        }
    }

    fn lock_vms(&self) -> MutexGuard<'_, HashMap<String, Arc<Vm>>> {
        self.vms.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn vm(&self, name: &str) -> Result<Arc<Vm>> {
        self.lock_vms()
            .get(name)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("unknown VM {name:?}")))
    }

    fn session(&self, name: &str, session: &str) -> Result<(Arc<Vm>, Arc<Session>)> {
        let vm = self.vm(name)?;
        let s = vm
            .sessions()
            .get(session)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("unknown session {session:?}")))?;
        Ok((vm, s))
    }

    /// Runs `fut` with this backend as microsandbox's default, for the
    /// static microsandbox calls (`Sandbox::builder`, `Snapshot::list`, ...).
    async fn scoped<F: Future>(&self, fut: F) -> F::Output {
        with_backend(Arc::clone(&self.local), fut).await
    }

    /// Boots VM `name` from `cfg.image`, replacing any VM of that name.
    async fn boot(&self, cfg: &Config, name: &str, network: bool) -> Result<Sandbox> {
        let cpus = u8::try_from(cfg.cpus).map_err(|_| {
            BackendError::InvalidArgument(format!("cpus: {} is more than 255", cfg.cpus))
        })?;
        let (image, memory, vm) = (cfg.image.clone(), cfg.memory_mib, name.to_string());
        let created = within(
            format!("booting {name:?} from {}", cfg.image),
            CREATE,
            self.scoped(async move {
                let mut b = Sandbox::builder(vm)
                    .image(image.as_str())
                    .cpus(cpus)
                    .memory(memory)
                    .replace();
                if !network {
                    b = b.disable_network();
                }
                b.create_detached().await
            }),
        )
        .await
        .and_then(|r| r.map_err(|e| internal(format!("booting {name:?} from {}", cfg.image), e)));
        if created.is_err() {
            self.destroy_named(name).await;
        }
        created
    }

    /// Restores VM `name` from template `template`'s snapshot.
    async fn restore(&self, cfg: &Config, name: &str, template: &str) -> Result<Sandbox> {
        let group = snapshot_group(template);
        let (vm, from, deny) = (name.to_string(), group.clone(), template_network(cfg));
        let restored = within(
            format!("restoring {name:?} from {group}"),
            RESTORE,
            self.scoped(async move {
                let mut b = Sandbox::restore(from).name(vm).cow_memory();
                if deny {
                    b = b.network_policy(NetworkPolicy::none());
                }
                b.restore().await
            }),
        )
        .await
        .and_then(|r| r.map_err(|e| internal(format!("restoring {name:?} from {group}"), e)));
        if restored.is_err() {
            self.destroy_named(name).await;
        }
        restored
    }

    /// Runs `setup` with `sh -c` in a VM that is not (yet) in the map.
    async fn run_setup(&self, sandbox: &Sandbox, name: &str, setup: &str) -> Result<()> {
        let spec = ExecSpec {
            command: "sh".into(),
            args: vec!["-c".into(), setup.into()],
            timeout_ms: u32::try_from(SETUP.as_millis()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        let r = self
            .run(name, sandbox, spec, false)
            .await
            .map_err(as_internal)?;
        if r.timed_out {
            return Err(BackendError::Internal(format!(
                "setup in {name:?} timed out after {SETUP:?}"
            )));
        }
        if r.exit_code != 0 {
            let tail = &r.stderr[r.stderr.len().saturating_sub(2048)..];
            return Err(BackendError::Internal(format!(
                "setup in {name:?} exited with {}: {}",
                r.exit_code,
                String::from_utf8_lossy(tail).trim_end()
            )));
        }
        Ok(())
    }

    /// Destroys a VM that is not in the map; logs failures.
    async fn destroy_sandbox(&self, sandbox: &Sandbox, name: &str) {
        match within("destroying", DESTROY, sandbox.destroy_with(FORCE_DESTROY)).await {
            Ok(Ok(())) | Ok(Err(MicrosandboxError::SandboxNotFound(_))) => {}
            Ok(Err(e)) => warn!(vm = %name, "destroying: {e}"),
            Err(e) => warn!(vm = %name, "{e}"),
        }
    }

    /// Destroys VM `name` by name, if it exists; logs failures. Cleans up
    /// after a failed or timed-out create or restore.
    async fn destroy_named(&self, name: &str) {
        let found = within("looking up", QUERY, self.scoped(Sandbox::get(name))).await;
        match found {
            Ok(Ok(handle)) => {
                match within("destroying", DESTROY, handle.destroy_with(FORCE_DESTROY)).await {
                    Ok(Ok(())) | Ok(Err(MicrosandboxError::SandboxNotFound(_))) => {}
                    Ok(Err(e)) => warn!(vm = %name, "destroying after a failed start: {e}"),
                    Err(e) => warn!(vm = %name, "destroying after a failed start: {e}"),
                }
            }
            Ok(Err(MicrosandboxError::SandboxNotFound(_))) => {}
            Ok(Err(e)) => warn!(vm = %name, "looking up after a failed start: {e}"),
            Err(e) => warn!(vm = %name, "looking up after a failed start: {e}"),
        }
    }

    /// Why the event stream of a command in VM `name` ended without an exit
    /// status: `NotFound` if the VM was destroyed (`tracked`: it was in the
    /// map) or stopped.
    async fn lost(&self, name: &str, sandbox: &Sandbox, tracked: bool) -> BackendError {
        if tracked && !self.lock_vms().contains_key(name) {
            return BackendError::NotFound(format!("VM {name:?} was destroyed"));
        }
        match within("probing the VM", QUERY, sandbox.status()).await {
            Ok(Ok(SandboxStatus::Running)) => BackendError::Internal(format!(
                "the command in {name:?} ended without an exit status"
            )),
            Ok(Ok(status)) => BackendError::NotFound(format!("VM {name:?} is {status:?}")),
            Ok(Err(e @ MicrosandboxError::SandboxNotFound(_))) => {
                BackendError::NotFound(e.to_string())
            }
            Ok(Err(e)) => internal(format!("the command in {name:?} ended; probing the VM"), e),
            Err(e) => e,
        }
    }

    /// Opens an exec stream; `stdin: None` pipes stdin.
    async fn open(
        &self,
        name: &str,
        sandbox: &Sandbox,
        spec: ExecSpec,
        stdin: Option<Vec<u8>>,
    ) -> Result<ExecHandle> {
        let ExecSpec {
            command,
            args,
            env,
            cwd,
            ..
        } = spec;
        let what = format!("starting {command:?} in {name:?}");
        let opened = within(
            &what,
            EXEC_START,
            sandbox.exec_stream_with(command.clone(), exec_options(args, env, cwd, stdin)),
        )
        .await?;
        match opened {
            Ok(handle) => Ok(handle),
            Err(MicrosandboxError::ExecFailed(f)) => Err(BackendError::InvalidArgument(format!(
                "starting {command:?}: {}",
                f.message
            ))),
            Err(MicrosandboxError::InvalidConfig(m)) => {
                Err(BackendError::InvalidArgument(format!("{what}: {m}")))
            }
            Err(
                e @ (MicrosandboxError::SandboxNotRunning(_)
                | MicrosandboxError::SandboxNotFound(_)),
            ) => Err(BackendError::NotFound(format!("{what}: {e}"))),
            Err(e) if !self.lock_vms().contains_key(name) => {
                Err(BackendError::NotFound(format!("{what}: {e}")))
            }
            Err(e) => Err(internal(what, e)),
        }
    }

    /// Runs a command to completion (see [`VmBackend::exec`]). `tracked`:
    /// the VM is in the map (not a template VM or one still starting).
    async fn run(
        &self,
        name: &str,
        sandbox: &Sandbox,
        spec: ExecSpec,
        tracked: bool,
    ) -> Result<ExecResult> {
        let start = Instant::now();
        let deadline = (spec.timeout_ms > 0)
            .then(|| tokio::time::Instant::now() + Duration::from_millis(spec.timeout_ms.into()));
        let command = spec.command.clone();
        let stdin = Some(spec.stdin.clone());
        let mut handle = self.open(name, sandbox, spec, stdin).await?;
        let mut guard = KillOnDrop(Some(handle.control()));

        let mut r = ExecResult::default();
        // Set once the command was killed for its timeout: how long to wait
        // for its exit status.
        let mut grace: Option<tokio::time::Instant> = None;
        let mut ended = false;
        let exit = loop {
            let event = match (grace, deadline) {
                (Some(grace), _) => match tokio::time::timeout_at(grace, handle.recv()).await {
                    Ok(event) => event,
                    Err(_) => break None,
                },
                (None, Some(deadline)) => tokio::select! {
                    event = handle.recv() => event,
                    () = tokio::time::sleep_until(deadline) => {
                        r.timed_out = true;
                        if let Err(e) = within("killing", KILL_GRACE, handle.kill()).await
                            .and_then(|r| r.map_err(|e| internal("killing", e)))
                        {
                            warn!(vm = %name, command = %command, "timeout kill: {e}");
                        }
                        grace = Some(tokio::time::Instant::now() + KILL_GRACE);
                        continue;
                    }
                },
                (None, None) => handle.recv().await,
            };
            match event {
                Some(ExecEvent::Stdout(data)) => {
                    push_capped(&mut r.stdout, &data, &mut r.truncated)
                }
                Some(ExecEvent::Stderr(data)) => {
                    push_capped(&mut r.stderr, &data, &mut r.truncated)
                }
                Some(ExecEvent::Exited { code }) => break Some(code),
                Some(ExecEvent::Failed(f)) => {
                    guard.disarm();
                    return Err(BackendError::InvalidArgument(format!(
                        "starting {command:?}: {}",
                        f.message
                    )));
                }
                Some(ExecEvent::Started { .. } | ExecEvent::StdinError(_)) => {}
                None => {
                    ended = true;
                    break None;
                }
            }
        };
        if exit.is_some() || ended {
            guard.disarm();
        }
        r.elapsed_us = micros(start.elapsed());
        match exit {
            Some(code) => {
                r.exit_code = if r.timed_out && code == 0 { -1 } else { code };
                Ok(r)
            }
            // Killed for its timeout, but no exit status followed (within the
            // grace period, or before the stream ended) on a VM still there.
            None if r.timed_out && (!ended || !tracked || self.lock_vms().contains_key(name)) => {
                r.exit_code = -1;
                Ok(r)
            }
            None => Err(self.lost(name, sandbox, tracked).await),
        }
    }

    async fn group_head(&self, group: &str) -> Result<String, MicrosandboxError> {
        let selector = group.to_string();
        match within(
            format!("reading the head of {group}"),
            SNAPSHOT_ADMIN,
            self.scoped(async move { Snapshot::group_head(&selector).await }),
        )
        .await
        {
            Ok(r) => r.map(|update| update.head),
            Err(e) => Err(MicrosandboxError::Runtime(e.to_string())),
        }
    }

    /// Removes the members of snapshot group `group` other than `keep`
    /// (all of them with `None`). Logs, and skips, members it cannot remove
    /// (for example while a VM restored from one still runs); fails if it
    /// cannot list them.
    async fn prune(&self, group: &str, keep: Option<&str>) -> Result<()> {
        let all = within(
            "listing snapshots",
            QUERY,
            self.scoped(async { Snapshot::list().await }),
        )
        .await?
        .map_err(|e| internal("listing snapshots", e))?;
        let mut failed = None;
        for h in all
            .iter()
            .filter(|h| h.group() == Some(group) && Some(h.id()) != keep)
        {
            let path = match h.path() {
                Ok(path) => path.to_string_lossy().into_owned(),
                Err(e) => {
                    warn!(snapshot = %h.id(), group, "no path, not removing: {e}");
                    continue;
                }
            };
            let removed = within(
                format!("removing snapshot {path}"),
                SNAPSHOT_ADMIN,
                self.scoped(async { Snapshot::remove(&path, true).await }),
            )
            .await
            .and_then(|r| r.map_err(|e| internal(format!("removing snapshot {path}"), e)));
            match removed {
                Ok(()) => debug!(snapshot = %h.id(), group, "removed old snapshot"),
                Err(e) => {
                    warn!(snapshot = %h.id(), group, "{e}");
                    failed = Some(e);
                }
            }
        }
        match (keep, failed) {
            // Removing everything (`remove_template`) must report failures.
            (None, Some(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

/// Points microsandbox at the runtime pair the `microsandbox` crate's build
/// script installed (`DSB_MSB_BUILD_HOME`, see `build.rs`), if it is still
/// there. `MSB_PATH` and `MSB_LIBKRUNFW_PATH` override it.
fn register_build_runtime() {
    let Some(home) = option_env!("DSB_MSB_BUILD_HOME") else {
        return;
    };
    let (msb, lib) = (
        Path::new(home).join("bin").join("msb"),
        Path::new(home).join("lib").join(LIBKRUNFW),
    );
    if msb.is_file() && lib.is_file() {
        // Set-once: later calls (a second backend) are ignored.
        microsandbox::config::set_sdk_msb_path(msb);
        microsandbox::set_libkrunfw_path(lib);
    }
}

#[async_trait]
impl VmBackend for MicrosandboxBackend {
    async fn prepare_template(&self, cfg: &Config, name: &str) -> Result<()> {
        let start = Instant::now();
        let (tpl, group) = (format!("{name}-tpl"), snapshot_group(name));
        let network = template_network(cfg);
        let mut timings = PrepareTimings::default();

        let sandbox = self.boot(cfg, &tpl, network).await.map_err(as_internal)?;
        timings.boot = start.elapsed();
        let captured = async {
            if !cfg.setup.is_empty() {
                let t = Instant::now();
                self.run_setup(&sandbox, &tpl, &cfg.setup).await?;
                timings.setup = t.elapsed();
            }
            let t = Instant::now();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let (alias, from, into) = (format!("{group}-{stamp}"), tpl.clone(), group.clone());
            let snap = within(
                format!("snapshotting {tpl:?}"),
                SNAPSHOT,
                self.scoped(async move {
                    Snapshot::builder(alias)
                        .from_sandbox(from)
                        .group(into)
                        .full()
                        .create()
                        .await
                }),
            )
            .await?
            .map_err(|e| internal(format!("snapshotting {tpl:?}"), e))?;
            timings.snapshot = t.elapsed();
            timings.snapshot_bytes = snap
                .size_bytes()
                .or_else(|| snap.path().ok().map(disk_usage));
            // Dropping it releases its read lease on the artifact.
            Ok::<_, BackendError>(snap.id().to_string())
        }
        .await;
        // The template VM has served its purpose either way.
        let t = Instant::now();
        self.destroy_sandbox(&sandbox, &tpl).await;
        timings.destroy = t.elapsed();
        let id = captured?;

        // A fresh boot does not descend from the group's previous head, so
        // publication keeps the old head: select the new one explicitly.
        let selector = format!("{group}:{id}");
        within(
            format!("selecting {selector}"),
            SNAPSHOT_ADMIN,
            self.scoped(async { Snapshot::group_head(&selector).await }),
        )
        .await?
        .map_err(|e| internal(format!("selecting {selector}"), e))?;
        if let Err(e) = self.prune(&group, Some(&id)).await {
            warn!(group, "removing old snapshots: {e}");
        }
        // Not restored from yet, so this keeps the new snapshot's RAM.
        self.prune_memory_cache().await;
        timings.total = start.elapsed();
        info!(template = %name, snapshot = %selector, network, boot_ms = millis(timings.boot),
            setup_ms = millis(timings.setup), snapshot_ms = millis(timings.snapshot),
            destroy_ms = millis(timings.destroy), total_ms = millis(timings.total),
            snapshot_bytes = ?timings.snapshot_bytes, "template prepared");
        *self.last_prepare.lock().unwrap_or_else(|e| e.into_inner()) = Some(timings);
        Ok(())
    }

    async fn spawn(&self, cfg: &Config, name: &str, from_template: Option<&str>) -> Result<()> {
        let start = Instant::now();
        let sandbox = match from_template {
            Some(t) => self.restore(cfg, name, t).await,
            None => self.boot(cfg, name, false).await,
        }
        .map_err(as_internal)?;
        if from_template.is_none()
            && !cfg.setup.is_empty()
            && let Err(e) = self.run_setup(&sandbox, name, &cfg.setup).await
        {
            self.destroy_sandbox(&sandbox, name).await;
            return Err(e);
        }
        let vm = Arc::new(Vm {
            sandbox,
            sessions: Mutex::new(HashMap::new()),
        });
        if self.lock_vms().insert(name.to_string(), vm).is_some() {
            warn!(vm = %name, "spawned over a VM of the same name");
        }
        debug!(vm = %name, template = ?from_template, elapsed_ms = millis(start.elapsed()), "spawned");
        Ok(())
    }

    async fn exec(&self, name: &str, spec: ExecSpec) -> Result<ExecResult> {
        if spec.command.is_empty() {
            return Err(BackendError::InvalidArgument("command is required".into()));
        }
        let vm = self.vm(name)?;
        self.run(name, &vm.sandbox, spec, true).await
    }

    async fn session_start(&self, name: &str, spec: ExecSpec) -> Result<String> {
        if spec.command.is_empty() {
            return Err(BackendError::InvalidArgument("command is required".into()));
        }
        let vm = self.vm(name)?;
        let command = spec.command.clone();
        // Like the process driver, sessions ignore the spec's stdin and
        // timeout: input comes from sends, and a session lives until closed.
        let mut handle = self.open(name, &vm.sandbox, spec, None).await?;
        let stdin = handle.take_stdin().ok_or_else(|| {
            BackendError::Internal(format!("starting {command:?}: no stdin pipe"))
        })?;
        let control = handle.control();
        let out = Arc::new(SessionOutput::default());
        let pump = tokio::spawn(pump(handle, Arc::clone(&out))).abort_handle();
        // From here, dropping `session` kills the process.
        let session = Arc::new(Session {
            stdin,
            control,
            out,
            pump,
            send: tokio::sync::Mutex::new(()),
        });

        let started = tokio::time::timeout(
            EXEC_START,
            session
                .out
                .wait_for(|c| c.started || c.failed.is_some() || c.ended),
        )
        .await;
        {
            let c = session.out.lock();
            if let Some(msg) = &c.failed {
                return Err(BackendError::InvalidArgument(format!(
                    "starting {command:?}: {msg}"
                )));
            }
            if started.is_err() {
                return Err(BackendError::Internal(format!(
                    "starting {command:?} in {name:?}: no start within {EXEC_START:?}"
                )));
            }
        }
        if !session.out.lock().started {
            return Err(self.lost(name, &vm.sandbox, true).await);
        }

        let id = format!("s{}", self.session_seq.fetch_add(1, Ordering::Relaxed) + 1);
        // Register only if the VM was not destroyed meanwhile.
        let vms = self.lock_vms();
        match vms.get(name) {
            Some(current) if Arc::ptr_eq(current, &vm) => {
                vm.sessions().insert(id.clone(), session);
                Ok(id)
            }
            _ => Err(BackendError::NotFound(format!("VM {name:?} was destroyed"))),
        }
    }

    async fn session_send(
        &self,
        name: &str,
        session: &str,
        input: &[u8],
        until: &[u8],
        timeout: Duration,
    ) -> Result<ExecResult> {
        let (vm, s) = self.session(name, session)?;
        let _send = s.send.lock().await;
        let start = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        let result = |timed_out: bool| {
            let mut c = s.out.lock();
            ExecResult {
                stdout: std::mem::take(&mut c.stdout),
                stderr: std::mem::take(&mut c.stderr),
                exit_code: c.exit.unwrap_or(0),
                truncated: std::mem::take(&mut c.truncated),
                timed_out,
                elapsed_us: micros(start.elapsed()),
            }
        };
        // A fresh capped window: output between sends is discarded.
        {
            let mut c = s.out.lock();
            c.stdout.clear();
            c.stderr.clear();
            c.truncated = false;
        }

        if !input.is_empty() {
            match tokio::time::timeout_at(deadline, s.stdin.write(input)).await {
                Err(_) => return Ok(result(true)),
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // A process that exited cannot take input: report the
                    // exit, as the process driver does.
                    let settle =
                        deadline.min(tokio::time::Instant::now() + Duration::from_millis(200));
                    let _ = tokio::time::timeout_at(
                        settle,
                        s.out.wait_for(|c| c.exit.is_some() || c.ended),
                    )
                    .await;
                    let (exited, ended) = {
                        let c = s.out.lock();
                        (c.exit.is_some(), c.ended)
                    };
                    if !exited {
                        if ended || !self.lock_vms().contains_key(name) {
                            return Err(self.lost(name, &vm.sandbox, true).await);
                        }
                        return Err(internal(format!("writing to session {session:?}"), e));
                    }
                }
            }
        }

        let (mut so, mut se) = (0, 0);
        let wait = s.out.wait_for(|c| {
            let hit = !until.is_empty()
                && (find_from(&c.stdout, until, &mut so) || find_from(&c.stderr, until, &mut se));
            hit || c.exit.is_some() || c.ended
        });
        let timed_out = tokio::time::timeout_at(deadline, wait).await.is_err() && !until.is_empty();
        let lost = {
            let c = s.out.lock();
            c.ended && c.exit.is_none()
        };
        if lost {
            return Err(self.lost(name, &vm.sandbox, true).await);
        }
        Ok(result(timed_out))
    }

    async fn session_close(&self, name: &str, session: &str) -> Result<()> {
        let vm = self.vm(name)?;
        let s = vm
            .sessions()
            .remove(session)
            .ok_or_else(|| BackendError::NotFound(format!("unknown session {session:?}")))?;
        if !s.out.lock().ended {
            if let Err(e) = within("killing", KILL_GRACE, s.control.kill())
                .await
                .and_then(|r| r.map_err(|e| internal("killing", e)))
            {
                warn!(vm = %name, session, "closing session: {e}");
            }
            let _ = tokio::time::timeout(KILL_GRACE, s.out.wait_for(|c| c.ended)).await;
        }
        Ok(())
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        let start = Instant::now();
        // Out of the map first: from now on, execs on it fail with NotFound.
        let vm = self.lock_vms().remove(name);
        let destroyed = match &vm {
            Some(vm) => {
                // The VM's end ends them; dropping them aborts their pumps.
                vm.sessions().clear();
                within(
                    format!("destroying {name:?}"),
                    DESTROY,
                    vm.sandbox.destroy_with(FORCE_DESTROY),
                )
                .await?
            }
            None => {
                // Not created by this process: a leftover of an earlier one.
                let found = within(
                    format!("looking up {name:?}"),
                    QUERY,
                    self.scoped(Sandbox::get(name)),
                )
                .await?;
                match found {
                    Ok(handle) => {
                        within(
                            format!("destroying {name:?}"),
                            DESTROY,
                            handle.destroy_with(FORCE_DESTROY),
                        )
                        .await?
                    }
                    Err(MicrosandboxError::SandboxNotFound(_)) => {
                        return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
                    }
                    Err(e) => return Err(internal(format!("looking up {name:?}"), e)),
                }
            }
        };
        match destroyed {
            Ok(()) => {}
            Err(MicrosandboxError::SandboxNotFound(_)) if vm.is_some() => {}
            Err(MicrosandboxError::SandboxNotFound(_)) => {
                return Err(BackendError::NotFound(format!("unknown VM {name:?}")));
            }
            Err(e) => return Err(internal(format!("destroying {name:?}"), e)),
        }
        debug!(vm = %name, elapsed_ms = millis(start.elapsed()), "destroyed");
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let after = cursor.take();
            let page = within(
                "listing VMs",
                QUERY,
                self.scoped(Sandbox::list_with(move |l| {
                    let l = l.limit(100);
                    match after {
                        Some(c) => l.cursor(c),
                        None => l,
                    }
                })),
            )
            .await?
            .map_err(|e| internal("listing VMs", e))?;
            names.extend(
                page.sandboxes
                    .iter()
                    .map(|s| s.name())
                    .filter(|n| n.starts_with(prefix))
                    .map(str::to_string),
            );
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped() {
        let mut buf = Vec::new();
        let mut truncated = false;
        push_capped(&mut buf, &vec![1; OUTPUT_CAP - 1], &mut truncated);
        assert!(!truncated);
        push_capped(&mut buf, &[2, 3], &mut truncated);
        assert_eq!(buf.len(), OUTPUT_CAP);
        assert_eq!(buf[OUTPUT_CAP - 1], 2);
        assert!(truncated);
        push_capped(&mut buf, &[4], &mut truncated);
        assert_eq!(buf.len(), OUTPUT_CAP);
    }

    #[test]
    fn find_from_spans_chunks() {
        let mut from = 0;
        let mut hay = b"abc>>".to_vec();
        assert!(!find_from(&hay, b">>> ", &mut from));
        assert_eq!(from, 2);
        hay.extend_from_slice(b"> x");
        assert!(find_from(&hay, b">>> ", &mut from));
        let mut from = 0;
        assert!(find_from(b"x", b"x", &mut from));
        assert_eq!(from, 1);
        assert!(!find_from(b"x", b"x", &mut from));
    }

    #[test]
    fn home_limit_matches_the_socket_layout() {
        // <home>/run/sandboxes/<24 hex>/control.sock must be < sun_path.
        let tail = "/run/sandboxes/".len() + 24 + "/control.sock".len();
        assert_eq!(tail, 52);
        let sun_path = if cfg!(target_os = "linux") { 108 } else { 104 };
        assert_eq!(MAX_MSB_HOME_LEN + tail, sun_path - 1);
    }
}
