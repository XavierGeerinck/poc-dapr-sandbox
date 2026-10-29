//! Pool behaviour against the FakeBackend (no hypervisor).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dapr_sandbox_microvm::backend::fake::FakeBackend;
use dapr_sandbox_microvm::backend::{BackendError, ExecResult, ExecSpec, VmBackend};
use dapr_sandbox_microvm::config::Config;
use dapr_sandbox_microvm::pool::{
    AcquirePath, Pool, PoolError, PoolOptions, template_name, vm_name, vm_prefix,
};

const WAIT: Duration = Duration::from_secs(5);

fn config(kv: &[(&str, &str)]) -> Config {
    let mut props: HashMap<String, String> = HashMap::from([("image".into(), "img".into())]);
    for (k, v) in kv {
        props.insert(k.to_string(), v.to_string());
    }
    Config::from_properties(&props).unwrap()
}

fn pool(fake: &Arc<FakeBackend>, component: &str, kv: &[(&str, &str)]) -> Arc<Pool> {
    pool_with(fake, component, kv, PoolOptions::default())
}

fn pool_with(
    fake: &Arc<FakeBackend>,
    component: &str,
    kv: &[(&str, &str)],
    opts: PoolOptions,
) -> Arc<Pool> {
    let backend: Arc<dyn VmBackend> = fake.clone();
    Pool::with_options(component, config(kv), backend, opts)
}

/// Polls `f` every 2 ms until it holds or `WAIT` passes.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn started(fake: &Arc<FakeBackend>, component: &str, kv: &[(&str, &str)]) -> Arc<Pool> {
    let p = pool(fake, component, kv);
    p.start();
    eventually("pool ready", || p.ready().0).await;
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_fills_then_ready() {
    let fake = Arc::new(FakeBackend::new());
    fake.pause_spawns();
    let p = pool(&fake, "comp", &[("warmPoolSize", "3")]);
    assert!(!p.ready().0);
    p.start();

    eventually("template prepared", || fake.prepared().len() == 1).await;
    for n in 1..=3 {
        let (ready, reason) = p.ready();
        assert!(!ready, "ready before spawn {n}: {reason}");
        fake.allow_spawns(1);
        eventually("spawn", || fake.spawned().len() == n).await;
    }
    eventually("ready after 3 spawns", || p.ready().0).await;
    let (_, reason) = p.ready();
    assert_eq!(reason, "warm pool 3/3");
    assert_eq!(p.stats().warm, 3);
    // Every warm VM was restored from the template.
    let tpl = template_name("comp");
    assert!(
        fake.spawned()
            .iter()
            .all(|(_, t)| t.as_deref() == Some(tpl.as_str()))
    );
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_reports_progress() {
    let fake = Arc::new(FakeBackend::new());
    fake.pause_spawns();
    let p = pool(&fake, "comp", &[("warmPoolSize", "4")]);
    p.start();
    eventually("template prepared", || fake.prepared().len() == 1).await;
    fake.allow_spawns(1);
    eventually("one warm", || p.stats().warm == 1).await;
    assert_eq!(p.ready(), (false, "warm pool 1/4".to_string()));
    fake.resume_spawns();
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_pool_then_restore() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "1")]).await;
    // Slow refills: the second acquire must find the pool empty.
    fake.set_spawn_delay(Duration::from_millis(300));

    let a = p.acquire().await.unwrap();
    assert_eq!(a.path, AcquirePath::Pool);
    let b = p.acquire().await.unwrap();
    assert_eq!(b.path, AcquirePath::Restore);
    assert_ne!(a.instance_id, b.instance_id);

    let tpl = template_name("comp");
    let spawned = fake.spawned();
    let inline = spawned.iter().find(|(n, _)| *n == b.instance_id).unwrap();
    assert_eq!(inline.1.as_deref(), Some(tpl.as_str()));
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn template_none_reports_boot() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("template", "none")],
    )
    .await;
    let a = p.acquire().await.unwrap();
    assert_eq!(a.path, AcquirePath::Boot);
    assert!(
        fake.prepared().is_empty(),
        "template=none prepares no template"
    );
    assert_eq!(fake.spawned(), vec![(a.instance_id.clone(), None)]);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_full() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "2"), ("maxSandboxesPerHost", "1")],
    )
    .await;
    let a = p.acquire().await.unwrap();
    assert_eq!(
        p.acquire().await.unwrap_err(),
        PoolError::HostFull { max: 1 }
    );
    // Releasing frees the slot.
    p.release(&a.instance_id).unwrap();
    p.acquire().await.unwrap();
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_full_counts_inline_spawns() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("maxSandboxesPerHost", "1")],
    )
    .await;
    fake.set_spawn_delay(Duration::from_millis(200));
    let p2 = p.clone();
    let first = tokio::spawn(async move { p2.acquire().await });
    eventually("inline spawn in flight", || p.stats().inline == 1).await;
    assert_eq!(
        p.acquire().await.unwrap_err(),
        PoolError::HostFull { max: 1 }
    );
    first.await.unwrap().unwrap();
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_zero_is_unlimited() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("maxSandboxesPerHost", "0")],
    )
    .await;
    for _ in 0..50 {
        p.acquire().await.unwrap();
    }
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_hit_does_not_wait_for_backend() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "4")]).await;
    // Refills and destroys are slow; a pool hit must not wait for either.
    fake.set_spawn_delay(Duration::from_secs(2));
    fake.set_destroy_delay(Duration::from_secs(2));
    for _ in 0..4 {
        let start = Instant::now();
        let a = p.acquire().await.unwrap();
        let took = start.elapsed();
        assert_eq!(a.path, AcquirePath::Pool);
        // Far below the 2 s backend delays; loose enough for a loaded CI box.
        assert!(took < Duration::from_millis(50), "pool hit took {took:?}");
        assert!(a.elapsed <= took);
    }
    fake.set_spawn_delay(Duration::ZERO);
    fake.set_destroy_delay(Duration::ZERO);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_claim_triggers_a_refill() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "2")]).await;
    p.acquire().await.unwrap();
    p.acquire().await.unwrap();
    eventually("refilled", || p.stats().warm == 2).await;
    assert_eq!(fake.spawned().len(), 4);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refill_respects_concurrency() {
    let fake = Arc::new(FakeBackend::new());
    fake.pause_spawns();
    let p = pool(
        &fake,
        "comp",
        &[("warmPoolSize", "5"), ("refillConcurrency", "2")],
    );
    p.start();
    eventually("two fills in flight", || p.stats().filling == 2).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(p.stats().filling, 2);
    fake.resume_spawns();
    eventually("ready", || p.ready().0).await;
    assert_eq!(fake.spawned().len(), 5);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_spawns_are_retried() {
    let fake = Arc::new(FakeBackend::new());
    fake.fail_next_spawns(3);
    let p = pool(&fake, "comp", &[("warmPoolSize", "2")]);
    p.start();
    eventually("ready despite failures", || p.ready().0).await;
    assert_eq!(fake.spawned().len(), 2);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_template_is_retried() {
    let fake = Arc::new(FakeBackend::new());
    // Two failures: the error is visible during 100 ms + 200 ms of backoff.
    fake.fail_next_prepares(2);
    let p = pool(&fake, "comp", &[("warmPoolSize", "1")]);
    p.start();
    eventually("prepare failure reported", || {
        p.ready().1.contains("injected prepare failure")
    })
    .await;
    eventually("ready after retry", || p.ready().0).await;
    assert_eq!(fake.prepared().len(), 1);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_acquire_waits_for_template() {
    let fake = Arc::new(FakeBackend::new());
    fake.set_prepare_delay(Duration::from_millis(100));
    let p = pool(&fake, "comp", &[("warmPoolSize", "0")]);
    p.start();
    let a = p.acquire().await.unwrap();
    assert_eq!(a.path, AcquirePath::Restore);
    assert_eq!(fake.prepared().len(), 1);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_is_async() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "1")]).await;
    let a = p.acquire().await.unwrap();
    fake.set_destroy_delay(Duration::from_secs(2));

    let start = Instant::now();
    p.release(&a.instance_id).unwrap();
    let took = start.elapsed();
    assert!(took < Duration::from_millis(20), "release took {took:?}");

    // Gone from the pool at once; destroyed in the background.
    assert_eq!(
        p.check(&a.instance_id).unwrap_err(),
        PoolError::NotFound(a.instance_id.clone())
    );
    // The destroy is under way (sleeping 2 s) but not done.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fake.destroyed().is_empty());
    assert!(fake.vms().contains(&a.instance_id));
    // Shutdown waits for it.
    p.shutdown().await;
    assert!(fake.destroyed().contains(&a.instance_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_unknown_is_not_found() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "1")]).await;
    assert_eq!(
        p.release("nope").unwrap_err(),
        PoolError::NotFound("nope".into())
    );
    // A warm, unclaimed VM is not acquired either.
    let warm = fake.vms().pop().unwrap();
    assert_eq!(p.release(&warm).unwrap_err(), PoolError::NotFound(warm));
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_reaps_leftovers() {
    let fake = Arc::new(FakeBackend::new());
    let prefix = vm_prefix("comp");
    let left = [format!("{prefix}000003"), format!("{prefix}tpl")];
    let other = format!("{}000001", vm_prefix("other"));
    for n in left.iter().chain([&other]) {
        fake.add_vm(n.clone());
    }
    fake.set_destroy_delay(Duration::from_millis(50));

    let p = pool(&fake, "comp", &[("warmPoolSize", "2")]);
    p.start();
    let deadline = Instant::now() + WAIT;
    while !p.ready().0 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Both destroyed by the time the pool is ready; other components untouched.
    let destroyed = fake.destroyed();
    for n in &left {
        assert!(destroyed.contains(n), "{n} not reaped: {destroyed:?}");
    }
    assert!(!destroyed.contains(&other));
    assert!(fake.vms().contains(&other));
    // New names never reuse a leftover's number.
    for (n, _) in fake.spawned() {
        assert!(n > left[0], "{n} reuses a leftover number");
    }
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_survives_list_failure() {
    let fake = Arc::new(FakeBackend::new());
    fake.set_fail_list(true);
    let p = started(&fake, "comp", &[("warmPoolSize", "1")]).await;
    assert_eq!(p.stats().warm, 1);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_releases_unknown() {
    let fake = Arc::new(FakeBackend::new());
    let opts = PoolOptions {
        reconcile_grace: Duration::ZERO,
        ..Default::default()
    };
    let p = pool_with(&fake, "comp", &[("warmPoolSize", "2")], opts);
    p.start();
    eventually("ready", || p.ready().0).await;
    let keep = p.acquire().await.unwrap().instance_id;
    let lost = p.acquire().await.unwrap().instance_id;
    eventually("refilled", || p.stats().warm == 2).await;

    assert_eq!(p.reconcile(std::slice::from_ref(&keep)), 1);
    p.check(&keep).unwrap();
    assert!(p.check(&lost).is_err());
    eventually("lost destroyed", || fake.destroyed().contains(&lost)).await;
    // Warm VMs are never released by reconcile.
    assert_eq!(p.stats().warm, 2);
    assert_eq!(p.reconcile(std::slice::from_ref(&keep)), 0);
    // Unknown IDs in `live` are ignored.
    assert_eq!(p.reconcile(&[keep.clone(), "ghost".into()]), 0);
    p.shutdown().await;
}

// The grace runs on tokio's clock, paused here, so the test steps it exactly.
#[tokio::test(start_paused = true)]
async fn reconcile_respects_grace() {
    let fake = Arc::new(FakeBackend::new());
    // Default grace: 60 s.
    let p = started(&fake, "comp", &[("warmPoolSize", "1")]).await;
    let a = p.acquire().await.unwrap().instance_id;
    tokio::time::advance(Duration::from_secs(30)).await;
    let b = p.acquire().await.unwrap().instance_id;
    assert_eq!(p.reconcile(&[]), 0, "both within the grace");

    tokio::time::advance(Duration::from_secs(29)).await; // a: 59 s, b: 29 s
    assert_eq!(p.reconcile(&[]), 0);
    tokio::time::advance(Duration::from_secs(1)).await; // a: 60 s, b: 30 s
    assert_eq!(p.reconcile(&[]), 1);
    assert!(p.check(&a).is_err());
    p.check(&b).unwrap();

    tokio::time::advance(Duration::from_secs(30)).await; // b: 60 s
    // A listed instance is kept however old.
    assert_eq!(p.reconcile(std::slice::from_ref(&b)), 0);
    assert_eq!(p.reconcile(&[]), 1);
    assert!(p.check(&b).is_err());

    // Warm VMs are never released.
    eventually("refilled", || p.stats().warm == 1).await;
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(p.reconcile(&[]), 0);
    assert_eq!(p.stats().warm, 1);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_destroys_all() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(&fake, "comp", &[("warmPoolSize", "2")]).await;
    p.acquire().await.unwrap();
    p.acquire().await.unwrap();
    // A refill is still in flight while shutting down.
    fake.set_spawn_delay(Duration::from_millis(100));
    p.acquire().await.unwrap();

    p.shutdown().await;
    assert!(fake.vms().is_empty(), "left behind: {:?}", fake.vms());
    assert_eq!(p.acquire().await.unwrap_err(), PoolError::Closed);
    let (ready, reason) = p.ready();
    assert!(!ready);
    assert_eq!(reason, "shut down");
    // Idempotent.
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_is_bounded() {
    let fake = Arc::new(FakeBackend::new());
    let opts = PoolOptions {
        shutdown_timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let p = pool_with(&fake, "comp", &[("warmPoolSize", "1")], opts);
    p.start();
    eventually("ready", || p.ready().0).await;
    fake.set_destroy_delay(Duration::from_secs(30));
    let start = Instant::now();
    p.shutdown().await;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_inline_acquire_keeps_the_vm() {
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("maxSandboxesPerHost", "1")],
    )
    .await;
    fake.set_spawn_delay(Duration::from_millis(100));
    // The caller gives up while the VM is being created.
    let r = tokio::time::timeout(Duration::from_millis(20), p.acquire()).await;
    assert!(r.is_err());
    // The VM lands in the warm pool instead of leaking, and frees its slot.
    eventually("vm kept warm", || p.stats().warm == 1).await;
    assert_eq!(p.stats().live, 0);
    assert_eq!(p.stats().inline, 0);
    let a = p.acquire().await.unwrap();
    assert_eq!(a.path, AcquirePath::Pool);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_acquire_frees_its_slot_during_startup() {
    let fake = Arc::new(FakeBackend::new());
    fake.set_prepare_delay(Duration::from_millis(500));
    let p = pool(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("maxSandboxesPerHost", "1")],
    );
    p.start();
    // A caller gives up while the template is still being prepared.
    let r = tokio::time::timeout(Duration::from_millis(20), p.acquire()).await;
    assert!(r.is_err());
    eventually("slot freed", || p.stats().inline == 0).await;
    assert!(!p.ready().0, "startup is still running");
    // So the host is not full for the next caller.
    let a = p.acquire().await.unwrap();
    assert_eq!(a.path, AcquirePath::Restore);
    assert_eq!(
        fake.spawned().len(),
        1,
        "the abandoned acquire spawned nothing"
    );
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_waits_for_the_previous_pool() {
    let fake = Arc::new(FakeBackend::new());
    fake.pause_spawns();
    // The old pool is inside two spawns (VMs 0 and 1), and its bounded
    // shutdown gives up on them.
    let opts = PoolOptions {
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    };
    let old = pool_with(&fake, "comp", &[("warmPoolSize", "2")], opts);
    old.start();
    // Inside the backend's spawn, not just scheduled to call it.
    eventually("old pool spawning", || fake.spawns_entered() == 2).await;
    old.shutdown().await;

    let new = pool(&fake, "comp", &[("warmPoolSize", "1")]);
    new.start_after(Some(old.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        new.ready(),
        (
            false,
            "waiting for the previous pool to shut down".to_string()
        )
    );
    assert_eq!(
        fake.prepared().len(),
        1,
        "the template is the old pool's alone"
    );

    fake.resume_spawns();
    eventually("new pool ready", || new.ready().0).await;
    // The old pool's late VMs were destroyed, and the new pool numbered past
    // them instead of reusing their names.
    let destroyed = fake.destroyed();
    for n in [vm_name("comp", 0), vm_name("comp", 1)] {
        assert!(destroyed.contains(&n), "{n} not destroyed: {destroyed:?}");
    }
    assert_eq!(fake.vms(), vec![vm_name("comp", 2)]);
    assert_eq!(fake.prepared().len(), 2);
    new.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_are_valid() {
    let re = regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").unwrap();
    let component = "comp";
    let fake = Arc::new(FakeBackend::new());
    let p = started(
        &fake,
        component,
        &[
            ("warmPoolSize", "0"),
            ("maxSandboxesPerHost", "0"),
            ("template", "none"),
        ],
    )
    .await;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let name = p.acquire().await.unwrap().instance_id;
        assert!(re.is_match(&name), "{name}");
        assert!(name.starts_with(&vm_prefix(component)));
        assert!(seen.insert(name));
    }
    p.shutdown().await;

    // The format, for any component name (never derived from sandbox IDs).
    for c in [
        "",
        "a",
        "my component/with spaces",
        "ünïcödé",
        &"x".repeat(500),
    ] {
        let prefix = vm_prefix(c);
        assert!(re.is_match(&template_name(c)), "{c}");
        assert!(re.is_match(&format!("{}tpl", prefix)), "{c}");
        for i in [0, 1, 999_999, 1_000_000, u64::MAX] {
            let n = vm_name(c, i);
            assert!(re.is_match(&n), "{n}");
            assert!(n.starts_with(&prefix));
        }
    }
    // sha256("comp") = f562267f...
    assert_eq!(vm_name("comp", 7), "dsb-f562267f-000007");
    assert_eq!(vm_prefix("comp"), "dsb-f562267f-");
    assert_eq!(template_name("comp"), "dsb-f562267f");
}

// Warm-up (`warmupCommand`): every VM the refill path adds to the warm pool
// has run the warm-up exactly once; inline spawns skip it.

/// The VM's execs, as argv.
fn argvs(fake: &FakeBackend, vm: &str) -> Vec<Vec<String>> {
    fake.execs_on(vm)
        .into_iter()
        .map(|s: ExecSpec| std::iter::once(s.command).chain(s.args).collect())
        .collect()
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refilled_vms_are_warmed_up_once_before_handout() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    fake.pause_execs();
    let p = pool(
        &fake,
        "comp",
        &[("warmPoolSize", "2"), ("warmupCommand", " echo warm\tup ")],
    );
    p.start();
    // Both VMs spawned, both warm-ups blocked: neither can be handed out.
    eventually("two warm-ups started", || fake.execs().len() == 2).await;
    let warming: Vec<String> = fake.execs().into_iter().map(|(vm, _)| vm).collect();
    assert_eq!(fake.spawned().len(), 2);
    assert_eq!(p.stats().warm, 0);
    let inline = p.acquire().await.unwrap();
    assert_eq!(inline.path, AcquirePath::Restore);
    assert!(!warming.contains(&inline.instance_id));

    fake.resume_execs();
    eventually("ready", || p.ready().0).await;
    for vm in &warming {
        assert_eq!(
            argvs(&fake, vm),
            vec![argv(&["echo", "warm", "up"])],
            "{vm}"
        );
        // Bounded, and nothing else in the spec.
        let spec = &fake.execs_on(vm)[0];
        assert!(spec.timeout_ms > 0, "{spec:?}");
        assert!(spec.env.is_empty() && spec.cwd.is_empty() && spec.stdin.is_empty());
    }
    // A pool hit hands out a warmed VM; the inline VM ran no warm-up.
    let hit = p.acquire().await.unwrap();
    assert_eq!(hit.path, AcquirePath::Pool);
    assert!(warming.contains(&hit.instance_id));
    assert!(fake.execs_on(&inline.instance_id).is_empty());
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_warmup_is_true() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    fake.set_exec_delay(Duration::from_millis(20));
    let p = started(&fake, "comp", &[("warmPoolSize", "3")]).await;
    let warm = fake.vms();
    assert_eq!(warm.len(), 3);
    for vm in &warm {
        assert_eq!(argvs(&fake, vm), vec![argv(&["true"])], "{vm}");
    }
    let s = p.stats();
    assert_eq!((s.warm, s.warmups, s.warmup_failures), (3, 3, 0));
    assert!(s.warmup_max >= Duration::from_millis(20), "{s:?}");
    assert!(s.warmup_total >= 3 * Duration::from_millis(20), "{s:?}");
    assert!(s.warmup_total >= s.warmup_max, "{s:?}");

    // Every refill is warmed up too.
    fake.set_exec_delay(Duration::ZERO);
    p.acquire().await.unwrap();
    eventually("refilled", || p.stats().warm == 3).await;
    assert_eq!(p.stats().warmups, 4);
    for vm in fake.vms() {
        assert_eq!(argvs(&fake, &vm), vec![argv(&["true"])], "{vm}");
    }
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_spawns_skip_the_warmup() {
    for template in ["snapshot", "none"] {
        let fake = Arc::new(FakeBackend::new());
        fake.record_execs();
        let p = started(
            &fake,
            "comp",
            &[("warmPoolSize", "0"), ("template", template)],
        )
        .await;
        let a = p.acquire().await.unwrap();
        assert_ne!(a.path, AcquirePath::Pool);
        assert!(fake.execs().is_empty(), "{template}: {:?}", fake.execs());
        assert_eq!(p.stats().warmups, 0);
        p.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_warmup_command_disables_it() {
    for v in ["", " \t "] {
        let fake = Arc::new(FakeBackend::new());
        fake.record_execs();
        let p = started(
            &fake,
            "comp",
            &[("warmPoolSize", "2"), ("warmupCommand", v)],
        )
        .await;
        assert_eq!(p.stats().warm, 2);
        assert_eq!(p.acquire().await.unwrap().path, AcquirePath::Pool);
        eventually("refilled", || p.stats().warm == 2).await;
        assert!(fake.execs().is_empty(), "{v:?}: {:?}", fake.execs());
        assert_eq!(p.stats().warmups, 0);
        p.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_waits_for_warmed_vms() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    fake.pause_execs();
    let p = pool(&fake, "comp", &[("warmPoolSize", "2")]);
    p.start();
    eventually("both spawned and warming", || fake.execs().len() == 2).await;
    assert_eq!(fake.spawned().len(), 2);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(p.ready(), (false, "warm pool 0/2".to_string()));
    fake.allow_execs(1);
    eventually("one warm", || p.stats().warm == 1).await;
    assert_eq!(p.ready(), (false, "warm pool 1/2".to_string()));
    fake.allow_execs(1);
    eventually("ready", || p.ready().0).await;
    assert_eq!(p.ready().1, "warm pool 2/2");
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_warmup_destroys_the_vm_and_is_retried() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    // The first three warm-ups fail: a non-zero exit, a backend error, a
    // timeout; later ones succeed.
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    fake.set_exec_handler(Arc::new(move |_, _| {
        match c.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(ExecResult {
                exit_code: 1,
                stderr: b"boom\n".to_vec(),
                ..Default::default()
            }),
            1 => Err(BackendError::Internal("agent went away".into())),
            2 => Ok(ExecResult {
                exit_code: -1,
                timed_out: true,
                ..Default::default()
            }),
            _ => Ok(ExecResult::default()),
        }
    }));
    let p = pool(
        &fake,
        "comp",
        &[("warmPoolSize", "2"), ("refillConcurrency", "1")],
    );
    let start = Instant::now();
    p.start();
    eventually("the failure is reported", || {
        let (ready, reason) = p.ready();
        !ready && reason.contains("last spawn failed: warm-up")
    })
    .await;
    eventually("ready after retries", || p.ready().0).await;
    // Counted as failed refills: the usual backoff, 100 + 200 + 400 ms.
    assert!(
        start.elapsed() >= Duration::from_millis(700),
        "{:?}",
        start.elapsed()
    );

    let spawned: Vec<String> = fake.spawned().into_iter().map(|(n, _)| n).collect();
    assert_eq!(spawned.len(), 5, "{spawned:?}");
    // Each VM whose warm-up failed was destroyed; only warmed VMs are left.
    assert_eq!(fake.destroyed(), spawned[..3]);
    assert_eq!(fake.vms(), spawned[3..]);
    let s = p.stats();
    assert_eq!((s.warm, s.warmups, s.warmup_failures), (2, 2, 3));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warmup_timeout_fails_the_refill() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    let opts = PoolOptions {
        warmup_timeout: Duration::from_millis(50),
        ..Default::default()
    };
    // The backend hangs, ignoring the spec's timeout.
    fake.set_exec_delay(Duration::from_secs(30));
    let p = pool_with(
        &fake,
        "comp",
        &[("warmPoolSize", "1"), ("refillConcurrency", "1")],
        opts,
    );
    p.start();
    eventually("the timeout is reported", || {
        p.ready().1.contains("timed out")
    })
    .await;
    let first = fake.spawned()[0].0.clone();
    eventually("its VM destroyed", || fake.destroyed().contains(&first)).await;
    assert!(p.stats().warmup_failures >= 1);
    // The spec carries the bound too, for a backend that honours it.
    assert_eq!(fake.execs()[0].1.timeout_ms, 50);

    fake.set_exec_delay(Duration::ZERO);
    eventually("ready", || p.ready().0).await;
    assert_eq!(fake.vms().len(), 1);
    assert!(!fake.vms().contains(&first));
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_during_warmup_leaks_nothing() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    fake.pause_execs();
    let p = pool(&fake, "comp", &[("warmPoolSize", "2")]);
    p.start();
    eventually("warming", || fake.execs().len() == 2).await;
    let start = Instant::now();
    p.shutdown().await;
    // The warm-ups are abandoned, not waited for (the bound is 20 s).
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(fake.vms().is_empty(), "left behind: {:?}", fake.vms());
    assert_eq!(fake.destroyed().len(), 2);
    assert_eq!(p.stats().warmups, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_inline_vm_is_warmed_before_it_joins_the_pool() {
    let fake = Arc::new(FakeBackend::new());
    fake.record_execs();
    let p = started(
        &fake,
        "comp",
        &[("warmPoolSize", "0"), ("maxSandboxesPerHost", "1")],
    )
    .await;
    fake.set_spawn_delay(Duration::from_millis(100));
    // The caller gives up while the VM is being created: the VM is kept for
    // the pool, and so warmed up first, like a refill.
    let r = tokio::time::timeout(Duration::from_millis(20), p.acquire()).await;
    assert!(r.is_err());
    eventually("vm kept warm", || p.stats().warm == 1).await;
    let vm = fake.vms()[0].clone();
    assert_eq!(argvs(&fake, &vm), vec![argv(&["true"])]);
    let a = p.acquire().await.unwrap();
    assert_eq!(
        (a.path, a.instance_id.as_str()),
        (AcquirePath::Pool, vm.as_str())
    );
    p.release(&a.instance_id).unwrap();

    // If that warm-up fails, the VM is destroyed instead.
    fake.set_exec_handler(Arc::new(|_, _| {
        Ok(ExecResult {
            exit_code: 1,
            ..Default::default()
        })
    }));
    let r = tokio::time::timeout(Duration::from_millis(20), p.acquire()).await;
    assert!(r.is_err());
    eventually("failure counted", || p.stats().warmup_failures == 1).await;
    let s = p.stats();
    assert_eq!((s.warm, s.live, s.inline), (0, 0, 0));
    eventually("vm destroyed", || fake.vms().is_empty()).await;
    p.shutdown().await;
}
