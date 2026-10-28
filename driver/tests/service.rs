//! The gRPC service over a temp Unix socket, with a tonic client, and the
//! binary itself.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dapr_sandbox_microvm::backend::fake::FakeBackend;
use dapr_sandbox_microvm::backend::{BackendError, ExecResult, VmBackend};
use dapr_sandbox_microvm::pool::{PoolOptions, vm_name, vm_prefix};
use dapr_sandbox_microvm::proto::sandbox_client::SandboxClient;
use dapr_sandbox_microvm::proto::*;
use dapr_sandbox_microvm::service::{COMPONENT_HEADER, SandboxService, serve};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Code, Request};
use tonic_reflection::pb::v1alpha::ServerReflectionRequest as V1AlphaReflectionRequest;
use tonic_reflection::pb::v1alpha::server_reflection_client::ServerReflectionClient as ReflectionV1Alpha;
use tonic_reflection::pb::v1alpha::server_reflection_request::MessageRequest as V1AlphaRequest;
use tonic_reflection::pb::v1alpha::server_reflection_response::MessageResponse as V1AlphaResponse;
use tower::service_fn;

const SANDBOX_SERVICE: &str = "dapr.proto.components.v1.Sandbox";
const WAIT: Duration = Duration::from_secs(5);

struct Harness {
    _dir: tempfile::TempDir,
    fake: Arc<FakeBackend>,
    service: SandboxService,
    client: SandboxClient<Channel>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_options(PoolOptions::default()).await
    }

    async fn with_options(opts: PoolOptions) -> Self {
        let dir = temp_dir();
        let path = dir.path().join("sb.sock");
        let fake = Arc::new(FakeBackend::new());
        let backend: Arc<dyn VmBackend> = fake.clone();
        let service = SandboxService::with_pool_options(backend, opts);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let svc = service.clone();
        let server = tokio::spawn(async move {
            serve(listener, svc, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        });
        let client = SandboxClient::new(connect(&path).await).max_decoding_message_size(64 << 20);
        Harness {
            _dir: dir,
            fake,
            service,
            client,
            stop: Some(stop),
            server: Some(server),
        }
    }

    async fn init(&mut self, component: &str, kv: &[(&str, &str)]) {
        let mut properties: HashMap<String, String> =
            HashMap::from([("image".into(), "img".into())]);
        for (k, v) in kv {
            properties.insert(k.to_string(), v.to_string());
        }
        let msg = SandboxInitRequest {
            metadata: Some(MetadataRequest { properties }),
        };
        self.client.init(req(component, msg)).await.unwrap();
    }

    async fn ready(&mut self, component: &str) -> SandboxReadyResponse {
        self.client
            .ready(req(component, SandboxReadyRequest {}))
            .await
            .unwrap()
            .into_inner()
    }

    async fn wait_ready(&mut self, component: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let r = self.ready(component).await;
            if r.ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{component} not ready: {}",
                r.reason
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn acquire(&mut self, component: &str) -> Result<SandboxAcquireResponse, tonic::Status> {
        let msg = SandboxAcquireRequest {
            sandbox_id: "sb-1".into(),
            activation_id: "act-1".into(),
        };
        self.client
            .acquire(req(component, msg))
            .await
            .map(|r| r.into_inner())
    }

    async fn exec(
        &mut self,
        component: &str,
        instance: &str,
        spec: SandboxExecSpec,
    ) -> Result<SandboxExecResponse, tonic::Status> {
        let msg = SandboxExecRequest {
            instance_id: instance.into(),
            spec: Some(spec),
        };
        self.client
            .exec(req(component, msg))
            .await
            .map(|r| r.into_inner())
    }

    async fn stop(mut self) {
        self.service.shutdown().await;
        let _ = self.stop.take().unwrap().send(());
        self.server.take().unwrap().await.unwrap();
    }
}

/// Polls `f` every 2 ms until it holds or `WAIT` passes.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn temp_dir() -> tempfile::TempDir {
    // Unix socket paths are limited to ~104 bytes on macOS: stay in $TMPDIR.
    tempfile::Builder::new().prefix("dsb").tempdir().unwrap()
}

async fn connect(path: &Path) -> Channel {
    let path = path.to_path_buf();
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move { Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?)) }
        }))
        .await
        .unwrap()
}

fn req<T>(component: &str, msg: T) -> Request<T> {
    let mut r = Request::new(msg);
    r.metadata_mut()
        .insert(COMPONENT_HEADER, component.parse().unwrap());
    r
}

fn cmd(command: &str, args: &[&str]) -> SandboxExecSpec {
    SandboxExecSpec {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_components_get_separate_pools() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1"), ("maxSandboxesPerHost", "1")])
        .await;
    h.init("b", &[("warmPoolSize", "2")]).await;
    h.wait_ready("a").await;
    h.wait_ready("b").await;
    assert_eq!(h.ready("b").await.reason, "warm pool 2/2");

    let a = h.acquire("a").await.unwrap();
    let b = h.acquire("b").await.unwrap();
    assert_eq!(a.path, SandboxAcquirePath::Pool as i32);
    assert_eq!(b.path, SandboxAcquirePath::Pool as i32);
    assert!(
        a.instance_id.starts_with(&vm_prefix("a")),
        "{}",
        a.instance_id
    );
    assert!(
        b.instance_id.starts_with(&vm_prefix("b")),
        "{}",
        b.instance_id
    );

    let out = h
        .exec("a", &a.instance_id, cmd("echo", &["hi"]))
        .await
        .unwrap();
    assert_eq!(out.stdout, b"hi\n");
    assert_eq!(out.exit_code, 0);

    // b's pool does not know a's instance.
    let err = h
        .exec("b", &a.instance_id, cmd("echo", &["hi"]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");

    // a is full; b is not.
    let err = h.acquire("a").await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
    h.acquire("b").await.unwrap();

    assert_eq!(h.service.pool("a").unwrap().stats().live, 1);
    assert_eq!(h.service.pool("b").unwrap().stats().live, 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_unknown_instance_is_not_found() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;

    let err = h.exec("a", "nope", cmd("echo", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    // A warm, unclaimed VM is not an acquired instance.
    let warm = h.fake.vms().pop().unwrap();
    let err = h.exec("a", &warm, cmd("echo", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    // A component that was never initialized (for example, after a driver
    // restart) knows no instances.
    let err = h.exec("zzz", "nope", cmd("echo", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_when_full_is_resource_exhausted() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "2"), ("maxSandboxesPerHost", "1")])
        .await;
    h.wait_ready("a").await;
    let first = h.acquire("a").await.unwrap();
    assert!(first.elapsed_us < 100_000, "{}", first.elapsed_us);
    let err = h.acquire("a").await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_paths_over_grpc() {
    let mut h = Harness::new().await;
    h.init("snap", &[("warmPoolSize", "0")]).await;
    h.init("boot", &[("warmPoolSize", "0"), ("template", "none")])
        .await;
    h.wait_ready("snap").await;
    h.wait_ready("boot").await;
    assert_eq!(
        h.acquire("snap").await.unwrap().path,
        SandboxAcquirePath::Restore as i32
    );
    assert_eq!(
        h.acquire("boot").await.unwrap().path,
        SandboxAcquirePath::Boot as i32
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_before_init_is_unavailable() {
    let mut h = Harness::new().await;
    let err = h.acquire("zzz").await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_before_init_is_false() {
    let mut h = Harness::new().await;
    let r = h.ready("zzz").await;
    assert!(!r.ready);
    assert!(r.reason.contains("not initialized"), "{}", r.reason);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_rejects_bad_metadata() {
    let mut h = Harness::new().await;
    let bad: &[&[(&str, &str)]] = &[
        &[],
        &[("image", "img"), ("template", "bogus")],
        &[("image", "img"), ("cpus", "-1")],
        &[("image", "img"), ("idleTimeout", "soon")],
    ];
    for kv in bad {
        let properties = kv
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let msg = SandboxInitRequest {
            metadata: Some(MetadataRequest { properties }),
        };
        let err = h.client.init(req("a", msg)).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{kv:?}: {err:?}");
        assert!(err.message().starts_with("sandbox: "), "{}", err.message());
    }
    assert!(h.service.pool("a").is_none());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_returns_before_the_pool_is_ready() {
    let mut h = Harness::new().await;
    h.fake.set_prepare_delay(Duration::from_millis(500));
    let start = Instant::now();
    h.init("a", &[("warmPoolSize", "1")]).await;
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "{:?}",
        start.elapsed()
    );
    // Startup moves through reaping to the 500 ms template preparation.
    let deadline = Instant::now() + WAIT;
    loop {
        let r = h.ready("a").await;
        assert!(!r.ready, "ready before the template: {}", r.reason);
        if r.reason == "preparing template" {
            break;
        }
        assert!(
            ["starting", "reaping leftover VMs"].contains(&r.reason.as_str()),
            "{}",
            r.reason
        );
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    h.wait_ready("a").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_again_replaces_the_pool() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let old = h.acquire("a").await.unwrap().instance_id;

    // daprd restarted and initializes the component again.
    h.init("a", &[("warmPoolSize", "2")]).await;
    h.wait_ready("a").await;
    assert_eq!(h.ready("a").await.reason, "warm pool 2/2");
    let err = h.exec("a", &old, cmd("echo", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    assert!(h.fake.destroyed().contains(&old));
    // Only the new pool's two warm VMs remain.
    assert_eq!(h.fake.vms().len(), 2, "{:?}", h.fake.vms());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_init_serialises_replacement() {
    let mut h = Harness::new().await;
    h.fake.pause_spawns();
    h.init("a", &[("warmPoolSize", "2")]).await;
    let p0 = h.service.pool("a").unwrap();
    // P0 is inside two spawns (VMs 0 and 1) when daprd initializes the
    // component twice more in quick succession (P1, then P2).
    eventually("P0 spawning", || {
        p0.stats().filling == 2 && h.fake.spawns_entered() == 2
    })
    .await;
    h.init("a", &[("warmPoolSize", "2")]).await;
    h.init("a", &[("warmPoolSize", "2")]).await;

    // P2 waits for P0's in-flight spawns (through P1, which never starts);
    // nothing touched P0's names or template meanwhile.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = h.ready("a").await;
    assert!(!r.ready);
    assert_eq!(r.reason, "waiting for the previous pool to shut down");
    assert_eq!(h.fake.prepared().len(), 1);

    h.fake.resume_spawns();
    h.wait_ready("a").await;
    // Every name was used once: P0's late VMs were destroyed and P2 numbered
    // past them. Only P0 and P2 prepared the template.
    let spawned: Vec<String> = h.fake.spawned().into_iter().map(|(n, _)| n).collect();
    let unique: HashSet<&String> = spawned.iter().collect();
    assert_eq!(unique.len(), spawned.len(), "{spawned:?}");
    assert_eq!(h.fake.prepared().len(), 2);
    assert_eq!(h.fake.vms(), vec![vm_name("a", 2), vm_name("a", 3)]);

    let id = h.acquire("a").await.unwrap().instance_id;
    let out = h.exec("a", &id, cmd("echo", &["ok"])).await.unwrap();
    assert_eq!(out.stdout, b"ok\n");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_on_demand_spawn_is_internal() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "0")]).await;
    h.wait_ready("a").await;
    // Whatever the backend calls it, a failed spawn is a driver failure:
    // daprd would read NOT_FOUND on Acquire as "invalid request".
    h.fake
        .fail_next_spawns_with(1, BackendError::NotFound("template vanished".into()));
    let err = h.acquire("a").await.unwrap_err();
    assert_eq!(err.code(), Code::Internal, "{err:?}");
    assert!(
        err.message().contains("template vanished"),
        "{}",
        err.message()
    );
    h.acquire("a").await.unwrap();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn component_header() {
    let mut h = Harness::new().await;
    let init = || SandboxInitRequest {
        metadata: Some(MetadataRequest {
            properties: HashMap::from([("image".to_string(), "img".to_string())]),
        }),
    };
    // Init needs a valid header: missing, empty or not visible ASCII.
    let err = h.client.init(Request::new(init())).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    let err = h.client.init(req("", init())).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    let mut r = Request::new(init());
    r.metadata_mut().insert(
        COMPONENT_HEADER,
        tonic::metadata::MetadataValue::try_from("ünï".as_bytes()).unwrap(),
    );
    let err = h.client.init(r).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(h.service.pool("").is_none());

    // Other RPCs without it behave as for a component never initialized.
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let ready = h
        .client
        .ready(Request::new(SandboxReadyRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(!ready.ready);
    assert!(ready.reason.contains(COMPONENT_HEADER), "{}", ready.reason);
    let err = h
        .client
        .acquire(Request::new(SandboxAcquireRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err:?}");
    let id = h.acquire("a").await.unwrap().instance_id;
    let exec = SandboxExecRequest {
        instance_id: id.clone(),
        spec: Some(cmd("echo", &[])),
    };
    let err = h.client.exec(Request::new(exec)).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    let rel = SandboxReleaseRequest { instance_id: id };
    let err = h.client.release(Request::new(rel)).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    let rec = SandboxReconcileRequest {
        live_instance_ids: vec![],
    };
    let res = h
        .client
        .reconcile(Request::new(rec))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.released, 0);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_validates_the_spec() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;
    let err = h.exec("a", &id, cmd("", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    let msg = SandboxExecRequest {
        instance_id: id.clone(),
        spec: None,
    };
    let err = h.client.exec(req("a", msg)).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_passes_the_spec_through() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;
    let spec = SandboxExecSpec {
        command: "cat".into(),
        stdin: b"from stdin".to_vec(),
        ..Default::default()
    };
    let out = h.exec("a", &id, spec).await.unwrap();
    assert_eq!(out.stdout, b"from stdin");
    let out = h.exec("a", &id, cmd("false", &[])).await.unwrap();
    assert_eq!(out.exit_code, 1);
    assert!(out.elapsed_us > 0);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_output_is_capped_per_stream() {
    let mut h = Harness::new().await;
    h.fake.set_exec_handler(Arc::new(|_, _| {
        Ok(ExecResult {
            stdout: vec![b'o'; 5 << 20],
            stderr: vec![b'e'; 10],
            ..Default::default()
        })
    }));
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;
    let out = h.exec("a", &id, cmd("big", &[])).await.unwrap();
    assert_eq!(out.stdout.len(), 4 << 20);
    assert_eq!(out.stderr.len(), 10);
    assert!(out.truncated);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_timeout_sets_timed_out() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;
    let spec = SandboxExecSpec {
        timeout_ms: 100,
        ..cmd("sleep", &["5"])
    };
    let start = Instant::now();
    let out = h.exec("a", &id, spec).await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(out.timed_out);
    assert_ne!(out.exit_code, 0);

    // A backend reporting a timeout with exit 0 still yields a non-zero exit.
    h.fake.set_exec_handler(Arc::new(|_, _| {
        Ok(ExecResult {
            timed_out: true,
            ..Default::default()
        })
    }));
    let out = h.exec("a", &id, cmd("x", &[])).await.unwrap();
    assert!(out.timed_out);
    assert_eq!(out.exit_code, -1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;

    let start = SandboxSessionStartRequest {
        instance_id: id.clone(),
        spec: Some(cmd("sh", &[])),
    };
    let sid = h
        .client
        .session_start(req("a", start))
        .await
        .unwrap()
        .into_inner()
        .session_id;
    assert!(!sid.is_empty());

    let send = |input: &[u8], until: &[u8], timeout_ms| SandboxSessionSendRequest {
        instance_id: id.clone(),
        session_id: sid.clone(),
        input: input.to_vec(),
        until: until.to_vec(),
        timeout_ms,
    };
    let out = h
        .client
        .session_send(req("a", send(b"hi\n", b"hi", 1000)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(out.stdout, b"hi\n");
    assert!(!out.timed_out);
    let out = h
        .client
        .session_send(req("a", send(b"x", b"never", 50)))
        .await
        .unwrap()
        .into_inner();
    assert!(out.timed_out);

    let close = SandboxSessionCloseRequest {
        instance_id: id.clone(),
        session_id: sid.clone(),
    };
    h.client
        .session_close(req("a", close.clone()))
        .await
        .unwrap();

    // Unknown sessions are NOT_FOUND.
    let err = h
        .client
        .session_send(req("a", send(b"x", b"", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    let err = h.client.session_close(req("a", close)).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    // So is a session on an unknown instance.
    let start = SandboxSessionStartRequest {
        instance_id: "nope".into(),
        spec: Some(cmd("sh", &[])),
    };
    let err = h.client.session_start(req("a", start)).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_and_reconcile() {
    let opts = PoolOptions {
        reconcile_grace: Duration::ZERO,
        ..Default::default()
    };
    let mut h = Harness::with_options(opts).await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    h.fake.set_destroy_delay(Duration::from_secs(1));

    let id = h.acquire("a").await.unwrap().instance_id;
    let start = Instant::now();
    let rel = SandboxReleaseRequest {
        instance_id: id.clone(),
    };
    h.client.release(req("a", rel.clone())).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "{:?}",
        start.elapsed()
    );
    let err = h.client.release(req("a", rel)).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound, "{err:?}");

    let keep = h.acquire("a").await.unwrap().instance_id;
    let lost = h.acquire("a").await.unwrap().instance_id;
    let rec = SandboxReconcileRequest {
        live_instance_ids: vec![keep.clone()],
    };
    let res = h
        .client
        .reconcile(req("a", rec))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.released, 1);
    let err = h.exec("a", &lost, cmd("echo", &[])).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    // Reconcile of an unknown component releases nothing.
    let rec = SandboxReconcileRequest {
        live_instance_ids: vec![],
    };
    let res = h
        .client
        .reconcile(req("zzz", rec))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.released, 0);
    h.fake.set_destroy_delay(Duration::ZERO);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_over_grpc_respects_grace() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    let id = h.acquire("a").await.unwrap().instance_id;
    let rec = SandboxReconcileRequest {
        live_instance_ids: vec![],
    };
    let res = h
        .client
        .reconcile(req("a", rec))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.released, 0);
    h.exec("a", &id, cmd("true", &[])).await.unwrap();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_destroys_every_pool() {
    let mut h = Harness::new().await;
    h.init("a", &[("warmPoolSize", "2")]).await;
    h.init("b", &[("warmPoolSize", "1")]).await;
    h.wait_ready("a").await;
    h.wait_ready("b").await;
    h.acquire("a").await.unwrap();
    let fake = h.fake.clone();
    h.stop().await;
    assert!(fake.vms().is_empty(), "{:?}", fake.vms());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn features_and_ping() {
    let mut h = Harness::new().await;
    let f = h
        .client
        .features(req("a", FeaturesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(f.features.is_empty());
    h.client.ping(req("a", PingRequest {})).await.unwrap();
    h.stop().await;
}

async fn list_services_v1alpha(channel: Channel) -> Vec<String> {
    let mut client = ReflectionV1Alpha::new(channel);
    let request = V1AlphaReflectionRequest {
        host: String::new(),
        message_request: Some(V1AlphaRequest::ListServices(String::new())),
    };
    let mut stream = client
        .server_reflection_info(tokio_stream::iter([request]))
        .await
        .unwrap()
        .into_inner();
    let res = stream.message().await.unwrap().unwrap();
    match res.message_response {
        Some(V1AlphaResponse::ListServicesResponse(l)) => {
            l.service.into_iter().map(|s| s.name).collect()
        }
        other => panic!("unexpected reflection response: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reflection_lists_the_sandbox_service() {
    let dir = temp_dir();
    let path = dir.path().join("sb.sock");
    let backend: Arc<dyn VmBackend> = Arc::new(FakeBackend::new());
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, SandboxService::new(backend), async {
        let _ = stopped.await;
    }));

    let services = list_services_v1alpha(connect(&path).await).await;
    assert!(
        services.iter().any(|s| s == SANDBOX_SERVICE),
        "{services:?}"
    );

    // v1 as well.
    use tonic_reflection::pb::v1::ServerReflectionRequest;
    use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
    use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
    use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
    let mut client = ServerReflectionClient::new(connect(&path).await);
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut stream = client
        .server_reflection_info(tokio_stream::iter([request]))
        .await
        .unwrap()
        .into_inner();
    let res = stream.message().await.unwrap().unwrap();
    let Some(MessageResponse::ListServicesResponse(l)) = res.message_response else {
        panic!("unexpected reflection response");
    };
    assert!(l.service.iter().any(|s| s.name == SANDBOX_SERVICE));

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}

// --- the binary ---

const BIN: &str = env!("CARGO_BIN_EXE_dapr-sandbox-microvm");

async fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while UnixStream::connect(path).await.is_err() {
        assert!(Instant::now() < deadline, "socket {path:?} never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("binary did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binary_serves_and_exits_on_sigterm() {
    const SECRET: &str = "s3cr3t-token-in-setup";
    let dir = temp_dir();
    let path: PathBuf = dir.path().join("microvm.sock");
    // A stale socket file from a previous run.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());

    let state = dir.path().join("state");
    let mut child = Command::new(BIN)
        .arg("--socket")
        .arg(&path)
        .args(["--backend", "fake", "--state-dir"])
        .arg(&state)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&path).await;

    let channel = connect(&path).await;
    let services = list_services_v1alpha(channel.clone()).await;
    assert!(
        services.iter().any(|s| s == SANDBOX_SERVICE),
        "{services:?}"
    );

    // The fake backend serves a component end to end. `setup` may carry
    // secrets, so no log line shows it.
    let mut client = SandboxClient::new(channel);
    let properties = HashMap::from([
        ("image".to_string(), "img".to_string()),
        ("warmPoolSize".to_string(), "1".to_string()),
        ("setup".to_string(), format!("echo {SECRET}")),
    ]);
    let init = SandboxInitRequest {
        metadata: Some(MetadataRequest { properties }),
    };
    client.init(req("microvm", init)).await.unwrap();
    let deadline = Instant::now() + WAIT;
    while !client
        .ready(req("microvm", SandboxReadyRequest {}))
        .await
        .unwrap()
        .into_inner()
        .ready
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let id = client
        .acquire(req("microvm", SandboxAcquireRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .instance_id;
    let exec = SandboxExecRequest {
        instance_id: id,
        spec: Some(cmd("echo", &["ok"])),
    };
    let out = client
        .exec(req("microvm", exec))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(out.stdout, b"ok\n");

    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let status = wait_exit(&mut child);
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "{status:?}\n{stderr}");
    assert!(!path.exists(), "socket left behind");
    assert!(stderr.contains("SIGTERM"), "{stderr}");
    assert!(stderr.contains("FakeBackend"), "{stderr}");
    assert!(stderr.contains("init"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");

    // Init persisted the component: a restarted driver serves it at once,
    // without an Init.
    let mut child = Command::new(BIN)
        .arg("--socket")
        .arg(&path)
        .args(["--backend", "fake", "--state-dir"])
        .arg(&state)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&path).await;
    let mut client = SandboxClient::new(connect(&path).await);
    let deadline = Instant::now() + WAIT;
    while !client
        .ready(req("microvm", SandboxReadyRequest {}))
        .await
        .unwrap()
        .into_inner()
        .ready
    {
        assert!(Instant::now() < deadline, "not restored");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    let status = wait_exit(&mut child);
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "{status:?}\n{stderr}");
    assert!(stderr.contains("restored persisted components"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
}

#[test]
fn binary_selftest_needs_the_microsandbox_backend() {
    let out = Command::new(BIN)
        .args(["--selftest", "python:3.12-slim", "--backend", "fake"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--selftest"), "{stderr}");
    assert!(stderr.contains("not available"), "{stderr}");
}

#[cfg(not(feature = "microsandbox"))]
#[test]
fn binary_without_the_feature_rejects_the_microsandbox_backend() {
    let out = Command::new(BIN)
        .args(["--backend", "microsandbox"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("microsandbox"), "{stderr}");
}

#[test]
fn binary_rejects_unknown_arguments() {
    let out = Command::new(BIN).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
