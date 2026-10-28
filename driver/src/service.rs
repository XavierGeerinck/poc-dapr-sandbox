//! The `dapr.proto.components.v1.Sandbox` gRPC service.
//!
//! daprd sets the [`COMPONENT_HEADER`] to the component name on every call;
//! the service keeps one [`Pool`] per component, which `Init` creates and
//! starts ([`SandboxService::replace_pool`]). Status codes follow the
//! contract and the Go process driver:
//!
//! | Situation | Code |
//! |---|---|
//! | unknown instance or session (also: component never initialized) | `NOT_FOUND` |
//! | host at `maxSandboxesPerHost` | `RESOURCE_EXHAUSTED` |
//! | bad metadata or exec spec; `Init` without a valid header | `INVALID_ARGUMENT` |
//! | Acquire before `Init` or after shutdown | `UNAVAILABLE` |
//! | backend failure, including any failed on-demand spawn | `INTERNAL` |
//!
//! Any other RPC without a valid header behaves as for a component that was
//! never initialized.
//!
//! `FAILED_PRECONDITION` is never used: daprd reserves it for "sandbox lost".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use crate::backend::{BackendError, ExecResult, ExecSpec, OUTPUT_CAP, VmBackend};
use crate::config::Config;
use crate::persist::StateDir;
use crate::pool::{AcquirePath, Pool, PoolError, PoolOptions};
use crate::proto::sandbox_server::{Sandbox, SandboxServer};
use crate::proto::*;

/// The request header daprd sets to the component name on every call.
pub const COMPONENT_HEADER: &str = "x-component-instance";

/// SessionSend timeout when `until` is set and `timeout_ms` is 0 (as in the
/// Go process driver).
pub const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest request the server decodes.
pub const MAX_REQUEST_SIZE: usize = 64 << 20;

impl From<BackendError> for Status {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::NotFound(m) => Status::not_found(m),
            BackendError::InvalidArgument(m) => Status::invalid_argument(m),
            BackendError::Internal(m) => Status::internal(m),
        }
    }
}

impl From<PoolError> for Status {
    fn from(e: PoolError) -> Self {
        match e {
            PoolError::HostFull { .. } => Status::resource_exhausted(e.to_string()),
            PoolError::NotFound(_) => Status::not_found(e.to_string()),
            PoolError::Closed => Status::unavailable(e.to_string()),
            // A failed spawn or template is a driver failure, whatever the
            // backend called it: daprd reads NOT_FOUND on Acquire as
            // "invalid request".
            PoolError::Backend(b) => Status::internal(b.to_string()),
        }
    }
}

struct Inner {
    backend: Arc<dyn VmBackend>,
    opts: PoolOptions,
    /// Never held across an await.
    pools: RwLock<HashMap<String, Arc<Pool>>>,
    /// Where `Init` persists each component's properties (restart
    /// recovery, see [`crate::persist`]); `None`: nowhere.
    state: Option<StateDir>,
    /// Orders `Init`s (and the startup restore), so that the persisted
    /// properties of a component are those of its newest pool. Never held
    /// across an await.
    init: Mutex<()>,
}

/// The Sandbox service: one [`Pool`] per component.
#[derive(Clone)]
pub struct SandboxService {
    inner: Arc<Inner>,
}

impl SandboxService {
    pub fn new(backend: Arc<dyn VmBackend>) -> Self {
        Self::with_pool_options(backend, PoolOptions::default())
    }

    pub fn with_pool_options(backend: Arc<dyn VmBackend>, opts: PoolOptions) -> Self {
        Self::build(backend, opts, None)
    }

    /// A service whose `Init` persists each component's properties in
    /// `state_dir`, for [`restore_persisted`](Self::restore_persisted)
    /// after a restart.
    pub fn with_state_dir(
        backend: Arc<dyn VmBackend>,
        opts: PoolOptions,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        Self::build(backend, opts, Some(StateDir::new(state_dir)))
    }

    fn build(backend: Arc<dyn VmBackend>, opts: PoolOptions, state: Option<StateDir>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                opts,
                pools: RwLock::new(HashMap::new()),
                state,
                init: Mutex::new(()),
            }),
        }
    }

    /// Starts a pool, through [`replace_pool`](Self::replace_pool), for
    /// every component persisted in the state directory that has none yet,
    /// so that a restarted driver is Ready without waiting for daprd's
    /// `Init`. Skips (with a warning) files whose properties no longer
    /// parse. Returns the number of pools started. Call it before serving.
    pub fn restore_persisted(&self) -> usize {
        let Some(state) = &self.inner.state else {
            return 0;
        };
        let persisted = match state.load() {
            Ok(p) => p,
            Err(e) => {
                warn!(dir = %state.dir().display(), "reading persisted components: {e}");
                return 0;
            }
        };
        let _init = self.inner.init.lock().unwrap_or_else(|e| e.into_inner());
        let mut started = 0;
        for p in persisted {
            if self.pool(&p.component).is_some() {
                continue;
            }
            match Config::from_properties(&p.properties) {
                Ok(cfg) => {
                    // Not `?cfg`: `setup` may carry secrets.
                    info!(component = %p.component, image = %cfg.image,
                        warm_pool_size = cfg.warm_pool_size, "restoring persisted component");
                    self.replace_pool(&p.component, cfg);
                    started += 1;
                }
                Err(e) => warn!(component = %p.component,
                    "persisted properties no longer valid, not restoring: {e}"),
            }
        }
        started
    }

    /// The pool of a component, if `Init` created one.
    pub fn pool(&self, component: &str) -> Option<Arc<Pool>> {
        self.inner
            .pools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(component)
            .cloned()
    }

    pub fn into_server(self) -> SandboxServer<Self> {
        SandboxServer::new(self)
    }

    /// Creates, registers and starts the pool for `component`, replacing any
    /// existing one. The replaced pool is closed at once (its VMs are
    /// destroyed); the new pool starts only after it, and every pool it
    /// replaced, has finished all its work, and continues their VM numbering,
    /// so pools of one component never overlap (see [`Pool::start_after`]).
    /// `Init` uses it; so can a restore of persisted components at startup.
    pub fn replace_pool(&self, component: &str, cfg: Config) -> Arc<Pool> {
        let pool = Pool::with_options(
            component,
            cfg,
            Arc::clone(&self.inner.backend),
            self.inner.opts.clone(),
        );
        let mut pools = self.inner.pools.write().unwrap_or_else(|e| e.into_inner());
        let old = pools.insert(component.to_string(), Arc::clone(&pool));
        if old.is_some() {
            info!(component, "replacing the existing pool");
        }
        // Under the map lock, so that replacements of one component chain in
        // map order and each new pool is started (and so waits for its
        // predecessor) before a later replacement can close it.
        pool.start_after(old);
        drop(pools);
        pool
    }

    /// Shuts down every pool (destroying all VMs), concurrently. A pool's
    /// bounded wait (`shutdown_timeout`) also covers the pools it replaced,
    /// since its startup waits for them.
    pub async fn shutdown(&self) {
        let pools: Vec<Arc<Pool>> = self
            .inner
            .pools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, p)| p)
            .collect();
        let mut set = tokio::task::JoinSet::new();
        for pool in pools {
            set.spawn(async move { pool.shutdown().await });
        }
        while set.join_next().await.is_some() {}
    }

    /// The caller's pool, or why there is none.
    fn pool_for<T>(&self, req: &Request<T>) -> Result<Arc<Pool>, String> {
        let component = component(req)
            .ok_or_else(|| format!("missing or invalid {COMPONENT_HEADER} header"))?;
        self.pool(&component)
            .ok_or_else(|| format!("component {component:?} is not initialized"))
    }

    /// The pool of the caller's component holding `instance_id`, or
    /// `NOT_FOUND`.
    fn instance_pool<T>(&self, req: &Request<T>, instance_id: &str) -> Result<Arc<Pool>, Status> {
        let pool = self
            .pool_for(req)
            .map_err(|why| Status::not_found(format!("unknown instance {instance_id:?}: {why}")))?;
        pool.check(instance_id)?;
        Ok(pool)
    }
}

/// The component name from the request header; `None` if the header is
/// missing, empty or not visible ASCII.
fn component<T>(req: &Request<T>) -> Option<String> {
    req.metadata()
        .get(COMPONENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

fn exec_spec(spec: Option<SandboxExecSpec>) -> Result<ExecSpec, Status> {
    let spec = spec.unwrap_or_default();
    if spec.command.is_empty() {
        return Err(Status::invalid_argument("command is required"));
    }
    Ok(ExecSpec {
        command: spec.command,
        args: spec.args,
        env: spec.env,
        cwd: spec.cwd,
        stdin: spec.stdin,
        timeout_ms: spec.timeout_ms,
    })
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Applies the contract to a backend result: the per-stream output cap, a
/// non-zero exit on timeout, and an elapsed time.
fn exec_response(mut r: ExecResult, measured: Duration) -> SandboxExecResponse {
    r.enforce_cap(OUTPUT_CAP);
    if r.timed_out && r.exit_code == 0 {
        r.exit_code = -1;
    }
    SandboxExecResponse {
        stdout: r.stdout,
        stderr: r.stderr,
        exit_code: r.exit_code,
        truncated: r.truncated,
        timed_out: r.timed_out,
        elapsed_us: if r.elapsed_us > 0 {
            r.elapsed_us
        } else {
            micros(measured)
        },
    }
}

/// An error from [`serve`].
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// Serves `service` plus gRPC reflection (v1, and v1alpha, which daprd's
/// pluggable component discovery uses) on `listener` until `shutdown`
/// resolves.
pub async fn serve<F>(
    listener: UnixListener,
    service: SandboxService,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: std::future::Future<Output = ()> + Send,
{
    let reflection_v1 = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let reflection_v1alpha = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1alpha()?;
    // tonic's default 4 MiB decode limit must not be what rejects a large
    // stdin or session input; daprd's own body limit applies first.
    let sandbox = service
        .into_server()
        .max_decoding_message_size(MAX_REQUEST_SIZE);
    tonic::transport::Server::builder()
        .add_service(sandbox)
        .add_service(reflection_v1)
        .add_service(reflection_v1alpha)
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await?;
    Ok(())
}

#[tonic::async_trait]
impl Sandbox for SandboxService {
    /// Parses the metadata and creates and starts the component's pool. It
    /// returns at once; the pool fills in the background and `Ready` reports
    /// progress. A pool that already exists for the component (daprd
    /// restarted) is replaced: see [`SandboxService::replace_pool`].
    async fn init(
        &self,
        req: Request<SandboxInitRequest>,
    ) -> Result<Response<SandboxInitResponse>, Status> {
        let component = component(&req).ok_or_else(|| {
            Status::invalid_argument(format!("missing or invalid {COMPONENT_HEADER} header"))
        })?;
        let props = req
            .into_inner()
            .metadata
            .map(|m| m.properties)
            .unwrap_or_default();
        let cfg =
            Config::from_properties(&props).map_err(|e| Status::invalid_argument(e.to_string()))?;
        // Not `?cfg`: `setup` may carry secrets.
        info!(component = %component, image = %cfg.image,
            warm_pool_size = cfg.warm_pool_size, "init");
        let _init = self.inner.init.lock().unwrap_or_else(|e| e.into_inner());
        // A failure only costs restart recovery: log it, keep the pool.
        if let Some(state) = &self.inner.state
            && let Err(e) = state.save(&component, &props)
        {
            error!(component = %component, dir = %state.dir().display(),
                "persisting the component's properties: {e}");
        }
        self.replace_pool(&component, cfg);
        Ok(Response::new(SandboxInitResponse {}))
    }

    async fn features(
        &self,
        _req: Request<FeaturesRequest>,
    ) -> Result<Response<FeaturesResponse>, Status> {
        Ok(Response::new(FeaturesResponse { features: vec![] }))
    }

    async fn ping(&self, _req: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {}))
    }

    async fn ready(
        &self,
        req: Request<SandboxReadyRequest>,
    ) -> Result<Response<SandboxReadyResponse>, Status> {
        let (ready, reason) = match self.pool_for(&req) {
            Ok(pool) => pool.ready(),
            Err(why) => (false, why),
        };
        Ok(Response::new(SandboxReadyResponse { ready, reason }))
    }

    async fn acquire(
        &self,
        req: Request<SandboxAcquireRequest>,
    ) -> Result<Response<SandboxAcquireResponse>, Status> {
        let pool = self.pool_for(&req).map_err(Status::unavailable)?;
        let component = pool.component();
        let r = req.into_inner();
        let a = pool.acquire().await.inspect_err(
            |e| debug!(component = %component, sandbox = %r.sandbox_id, "acquire failed: {e}"),
        )?;
        debug!(component = %component, sandbox = %r.sandbox_id, activation = %r.activation_id,
            instance = %a.instance_id, path = ?a.path, elapsed_us = micros(a.elapsed), "acquired");
        let path = match a.path {
            AcquirePath::Pool => SandboxAcquirePath::Pool,
            AcquirePath::Restore => SandboxAcquirePath::Restore,
            AcquirePath::Boot => SandboxAcquirePath::Boot,
        };
        Ok(Response::new(SandboxAcquireResponse {
            instance_id: a.instance_id,
            path: path.into(),
            elapsed_us: micros(a.elapsed),
        }))
    }

    async fn exec(
        &self,
        req: Request<SandboxExecRequest>,
    ) -> Result<Response<SandboxExecResponse>, Status> {
        self.instance_pool(&req, &req.get_ref().instance_id)?;
        let r = req.into_inner();
        let spec = exec_spec(r.spec)?;
        let start = Instant::now();
        let res = self.inner.backend.exec(&r.instance_id, spec).await?;
        Ok(Response::new(exec_response(res, start.elapsed())))
    }

    async fn session_start(
        &self,
        req: Request<SandboxSessionStartRequest>,
    ) -> Result<Response<SandboxSessionStartResponse>, Status> {
        self.instance_pool(&req, &req.get_ref().instance_id)?;
        let r = req.into_inner();
        let spec = exec_spec(r.spec)?;
        let session_id = self
            .inner
            .backend
            .session_start(&r.instance_id, spec)
            .await?;
        Ok(Response::new(SandboxSessionStartResponse { session_id }))
    }

    async fn session_send(
        &self,
        req: Request<SandboxSessionSendRequest>,
    ) -> Result<Response<SandboxExecResponse>, Status> {
        self.instance_pool(&req, &req.get_ref().instance_id)?;
        let r = req.into_inner();
        let timeout = if r.timeout_ms == 0 && !r.until.is_empty() {
            DEFAULT_SEND_TIMEOUT
        } else {
            Duration::from_millis(r.timeout_ms.into())
        };
        let start = Instant::now();
        let res = self
            .inner
            .backend
            .session_send(&r.instance_id, &r.session_id, &r.input, &r.until, timeout)
            .await?;
        Ok(Response::new(exec_response(res, start.elapsed())))
    }

    async fn session_close(
        &self,
        req: Request<SandboxSessionCloseRequest>,
    ) -> Result<Response<SandboxSessionCloseResponse>, Status> {
        self.instance_pool(&req, &req.get_ref().instance_id)?;
        let r = req.into_inner();
        self.inner
            .backend
            .session_close(&r.instance_id, &r.session_id)
            .await?;
        Ok(Response::new(SandboxSessionCloseResponse {}))
    }

    /// Returns at once; the VM is destroyed in the background.
    async fn release(
        &self,
        req: Request<SandboxReleaseRequest>,
    ) -> Result<Response<SandboxReleaseResponse>, Status> {
        let pool = self.instance_pool(&req, &req.get_ref().instance_id)?;
        pool.release(&req.get_ref().instance_id)?;
        Ok(Response::new(SandboxReleaseResponse {}))
    }

    async fn reconcile(
        &self,
        req: Request<SandboxReconcileRequest>,
    ) -> Result<Response<SandboxReconcileResponse>, Status> {
        let released = match self.pool_for(&req) {
            Ok(pool) => pool.reconcile(&req.get_ref().live_instance_ids),
            Err(_) => 0,
        };
        Ok(Response::new(SandboxReconcileResponse { released }))
    }
}
