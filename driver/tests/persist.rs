//! Restart recovery: `Init` persists a component's properties, and a new
//! service on the same state directory restores its pool without an `Init`.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dapr_sandbox_microvm::backend::VmBackend;
use dapr_sandbox_microvm::backend::fake::FakeBackend;
use dapr_sandbox_microvm::persist::{Persisted, StateDir};
use dapr_sandbox_microvm::pool::PoolOptions;
use dapr_sandbox_microvm::proto::sandbox_server::Sandbox;
use dapr_sandbox_microvm::proto::{MetadataRequest, SandboxInitRequest};
use dapr_sandbox_microvm::service::{COMPONENT_HEADER, SandboxService};
use tonic::Request;

const WAIT: Duration = Duration::from_secs(5);

fn props(kv: &[(&str, &str)]) -> HashMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn service(dir: &std::path::Path) -> (Arc<FakeBackend>, SandboxService) {
    let fake = Arc::new(FakeBackend::new());
    let backend: Arc<dyn VmBackend> = fake.clone();
    let svc = SandboxService::with_state_dir(backend, PoolOptions::default(), dir);
    (fake, svc)
}

async fn init(svc: &SandboxService, component: &str, properties: HashMap<String, String>) {
    let mut req = Request::new(SandboxInitRequest {
        metadata: Some(MetadataRequest { properties }),
    });
    req.metadata_mut()
        .insert(COMPONENT_HEADER, component.parse().unwrap());
    svc.init(req).await.unwrap();
}

async fn wait_ready(svc: &SandboxService, component: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let (ready, reason) = svc.pool(component).expect("no pool").ready();
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "{component} not ready: {reason}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn state(dir: &std::path::Path) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("state")
        .tempdir_in(dir)
        .unwrap()
}

#[tokio::test]
async fn init_persists_the_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("components");
    let (_fake, svc) = service(&dir);
    let p = props(&[
        ("image", "img"),
        ("warmPoolSize", "1"),
        ("setup", "echo secret"),
    ]);
    init(&svc, "comp-a", p.clone()).await;

    let sd = StateDir::new(&dir);
    let file = sd.path_for("comp-a");
    assert!(file.exists());
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        sd.load().unwrap(),
        vec![Persisted {
            component: "comp-a".into(),
            properties: p
        }]
    );

    // A later Init overwrites it; no temp files remain.
    let p2 = props(&[("image", "img2")]);
    init(&svc, "comp-a", p2.clone()).await;
    assert_eq!(sd.load().unwrap()[0].properties, p2);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");

    // An invalid Init persists nothing.
    let mut req = Request::new(SandboxInitRequest {
        metadata: Some(MetadataRequest {
            properties: props(&[("image", "")]),
        }),
    });
    req.metadata_mut()
        .insert(COMPONENT_HEADER, "comp-b".parse().unwrap());
    assert!(svc.init(req).await.is_err());
    assert!(!sd.path_for("comp-b").exists());
    svc.shutdown().await;
}

#[tokio::test]
async fn a_new_service_restores_persisted_pools_without_init() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = state(tmp.path());
    let (_fake, svc) = service(dir.path());
    init(
        &svc,
        "comp-a",
        props(&[("image", "img"), ("warmPoolSize", "2")]),
    )
    .await;
    init(
        &svc,
        "comp/b with spaces",
        props(&[("image", "img"), ("template", "none")]),
    )
    .await;
    wait_ready(&svc, "comp-a").await;
    svc.shutdown().await;
    drop(svc);

    // "Restart": a new service and backend on the same state directory.
    let (fake, svc) = service(dir.path());
    assert!(svc.pool("comp-a").is_none());
    assert_eq!(svc.restore_persisted(), 2);
    wait_ready(&svc, "comp-a").await;
    wait_ready(&svc, "comp/b with spaces").await;
    assert_eq!(svc.pool("comp-a").unwrap().stats().warm, 2);
    assert_eq!(fake.prepared().len(), 1, "only comp-a uses a template");
    // Restoring again starts nothing: the pools exist.
    assert_eq!(svc.restore_persisted(), 0);
    svc.shutdown().await;
}

/// `warmupCommand` is persisted with the other properties, and a restored
/// pool keeps the difference between an absent key (`true`) and an empty one
/// (disabled).
#[tokio::test]
async fn restored_pools_keep_their_warmup_command() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = state(tmp.path());
    let (_fake, svc) = service(dir.path());
    let cases = [
        ("absent", props(&[("image", "img"), ("warmPoolSize", "1")])),
        (
            "empty",
            props(&[
                ("image", "img"),
                ("warmPoolSize", "1"),
                ("warmupCommand", ""),
            ]),
        ),
        (
            "custom",
            props(&[
                ("image", "img"),
                ("warmPoolSize", "1"),
                ("warmupCommand", "echo warm"),
            ]),
        ),
    ];
    for (component, p) in &cases {
        init(&svc, component, p.clone()).await;
    }
    svc.shutdown().await;
    drop(svc);
    let persisted = StateDir::new(dir.path()).load().unwrap();
    let empty = persisted.iter().find(|p| p.component == "empty").unwrap();
    assert_eq!(
        empty.properties.get("warmupCommand").map(String::as_str),
        Some("")
    );
    let absent = persisted.iter().find(|p| p.component == "absent").unwrap();
    assert!(!absent.properties.contains_key("warmupCommand"));

    let (fake, svc) = service(dir.path());
    fake.record_execs();
    assert_eq!(svc.restore_persisted(), 3);
    for (component, want) in [
        ("absent", vec!["true"]),
        ("empty", vec![]),
        ("custom", vec!["echo", "warm"]),
    ] {
        let pool = svc.pool(component).unwrap();
        assert_eq!(pool.config().warmup_command, want, "{component}");
        wait_ready(&svc, component).await;
        let vm = fake
            .vms()
            .into_iter()
            .find(|vm| vm.starts_with(pool.prefix()))
            .unwrap();
        let argvs: Vec<Vec<String>> = fake
            .execs_on(&vm)
            .into_iter()
            .map(|s| std::iter::once(s.command).chain(s.args).collect())
            .collect();
        let expected: Vec<Vec<String>> = if want.is_empty() {
            vec![]
        } else {
            vec![want.iter().map(|w| w.to_string()).collect()]
        };
        assert_eq!(argvs, expected, "{component}");
    }
    svc.shutdown().await;
}

#[tokio::test]
async fn restore_skips_bad_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = state(tmp.path());
    let sd = StateDir::new(dir.path());
    sd.save("good", &props(&[("image", "img")])).unwrap();
    // Properties that no longer parse.
    sd.save("bad-config", &props(&[("image", "img"), ("cpus", "x")]))
        .unwrap();
    // Not JSON; another component's content under this name; temp files.
    std::fs::write(sd.path_for("garbage"), b"{not json").unwrap();
    std::fs::copy(sd.path_for("good"), sd.path_for("impostor")).unwrap();
    std::fs::write(dir.path().join(".x.json.1-0.tmp"), b"{}").unwrap();
    std::fs::write(dir.path().join("README"), b"hi").unwrap();

    let loaded: Vec<String> = sd
        .load()
        .unwrap()
        .into_iter()
        .map(|p| p.component)
        .collect();
    assert_eq!(loaded, ["bad-config", "good"]);

    let (_fake, svc) = service(dir.path());
    assert_eq!(svc.restore_persisted(), 1);
    assert!(svc.pool("good").is_some());
    assert!(svc.pool("bad-config").is_none());
    svc.shutdown().await;
}

#[tokio::test]
async fn without_a_state_dir_nothing_is_persisted() {
    let svc = SandboxService::new(Arc::new(FakeBackend::new()));
    init(&svc, "comp-a", props(&[("image", "img")])).await;
    assert_eq!(svc.restore_persisted(), 0);
    svc.shutdown().await;

    // A missing state directory holds nothing.
    let tmp = tempfile::tempdir().unwrap();
    let (_fake, svc) = service(&tmp.path().join("missing"));
    assert_eq!(svc.restore_persisted(), 0);
}
