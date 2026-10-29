//! Live tests of the microsandbox backend. They boot real microVMs (a
//! hypervisor, the `python:3.12-slim` image, a few GiB of RAM), so every test
//! is `#[ignore]`d:
//!
//! ```sh
//! cargo test --manifest-path driver/Cargo.toml --test microsandbox_live -- --ignored --nocapture
//! ```
//!
//! They use their own microsandbox home, `/tmp/dsb-live-<uid>` (override with
//! `DSB_LIVE_MSB_HOME`), never the user's: it must be short, because
//! microsandbox puts Unix sockets under it (see `MAX_MSB_HOME_LEN`). The
//! tests share one image cache there and run one at a time (a process-wide
//! lock), so that timings are not skewed by each other. VM and template
//! names follow the pool's (`dsb-<hash>-NNNNNN`), one component per test.
//! `live_network_isolation` also needs Internet access from the host.

#![cfg(feature = "microsandbox")]

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dapr_sandbox_microvm::backend::{BackendError, ExecSpec, OUTPUT_CAP, VmBackend};
use dapr_sandbox_microvm::config::Config;
use dapr_sandbox_microvm::microsandbox_backend::{MAX_MSB_HOME_LEN, MicrosandboxBackend};
use dapr_sandbox_microvm::pool::{AcquirePath, Pool, template_name, vm_name, vm_prefix};
use dapr_sandbox_microvm::selftest::{ms, percentile};
use dapr_sandbox_microvm::service::SandboxService;
use sha2::Digest;

const IMAGE: &str = "python:3.12-slim";

/// One live test at a time.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn live_home() -> PathBuf {
    if let Some(h) = std::env::var_os("DSB_LIVE_MSB_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    let uid = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let home = PathBuf::from(format!("/tmp/dsb-live-{}", uid.trim()));
    assert!(home.as_os_str().len() <= MAX_MSB_HOME_LEN);
    match std::fs::create_dir(&home) {
        Ok(()) => std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap(),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => panic!("creating {}: {e}", home.display()),
    }
    // /tmp is shared: refuse a directory someone else planted.
    let md = std::fs::symlink_metadata(&home).unwrap();
    assert!(md.is_dir(), "{} is not a directory", home.display());
    assert_eq!(
        md.uid().to_string(),
        uid.trim(),
        "{} belongs to someone else",
        home.display()
    );
    assert_eq!(
        md.mode() & 0o077,
        0,
        "{} is accessible to others",
        home.display()
    );
    home
}

async fn backend() -> Arc<MicrosandboxBackend> {
    Arc::new(MicrosandboxBackend::new(live_home()).await.unwrap())
}

fn cfg(kv: &[(&str, &str)]) -> Config {
    let mut props = HashMap::from([("image".to_string(), IMAGE.to_string())]);
    for (k, v) in kv {
        props.insert(k.to_string(), v.to_string());
    }
    Config::from_properties(&props).unwrap()
}

fn spec(command: &str, args: &[&str]) -> ExecSpec {
    ExecSpec {
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        ..Default::default()
    }
}

/// RAM images realized for copy-on-write restores (see
/// `MicrosandboxBackend::remove_template`).
fn ram_images(home: &std::path::Path) -> usize {
    std::fs::read_dir(home.join("cache/memory/snapshots"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".ram"))
            })
            .count()
        })
        .unwrap_or(0)
}

/// Prepares the component's template, timing it, after destroying VMs a
/// failed earlier run left behind (as a pool's startup does).
async fn prepare(b: &MicrosandboxBackend, component: &str, cfg: &Config) -> String {
    for vm in b.list(&vm_prefix(component)).await.unwrap() {
        b.destroy(&vm).await.unwrap();
    }
    let t = template_name(component);
    let start = Instant::now();
    b.prepare_template(cfg, &t).await.unwrap();
    println!(
        "{component}: prepare_template {:.0} ms",
        ms(start.elapsed())
    );
    t
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_selftest() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let ram_before = ram_images(b.home());
    let t = prepare(&b, "live-selftest", &c).await;
    let vm = vm_name("live-selftest", 1);

    let start = Instant::now();
    b.spawn(&c, &vm, Some(&t)).await.unwrap();
    println!("restore {:.0} ms", ms(start.elapsed()));

    let r = b.exec(&vm, spec("echo", &["ok"])).await.unwrap();
    assert_eq!(r.stdout, b"ok\n");
    assert_eq!(r.exit_code, 0);
    assert!(!r.timed_out && !r.truncated);

    // The exec relay alone, on a running VM.
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let r = b.exec(&vm, spec("echo", &["ok"])).await.unwrap();
        samples.push(start.elapsed());
        assert_eq!(r.stdout, b"ok\n");
    }
    println!(
        "exec echo ok x20: p50 {:.2} ms, p90 {:.2} ms, max {:.2} ms",
        ms(percentile(&samples, 50.0)),
        ms(percentile(&samples, 90.0)),
        ms(percentile(&samples, 100.0))
    );

    // Stdin, env, cwd and exit codes.
    let mut s = spec("sh", &["-c", "cat; echo \"$FOO\"; pwd; exit 3"]);
    s.stdin = b"in\n".to_vec();
    s.env = HashMap::from([("FOO".into(), "bar".into())]);
    s.cwd = "/tmp".into();
    let r = b.exec(&vm, s).await.unwrap();
    assert_eq!(r.stdout, b"in\nbar\n/tmp\n");
    assert_eq!(r.exit_code, 3);

    // A command that cannot start is the caller's mistake.
    let e = b.exec(&vm, spec("no-such-command", &[])).await.unwrap_err();
    assert!(matches!(e, BackendError::InvalidArgument(_)), "{e:?}");

    b.destroy(&vm).await.unwrap();
    let e = b.exec(&vm, spec("echo", &["ok"])).await.unwrap_err();
    assert!(matches!(e, BackendError::NotFound(_)), "{e:?}");
    let e = b.destroy(&vm).await.unwrap_err();
    assert!(matches!(e, BackendError::NotFound(_)), "{e:?}");
    b.remove_template(&t).await.unwrap();
    // The restore's RAM image went with the template.
    let ram_after = ram_images(b.home());
    println!("RAM images: {ram_before} before, {ram_after} after");
    assert!(ram_after <= ram_before);
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_session_turn2() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-session", &c).await;
    let vm = vm_name("live-session", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();

    let sid = b
        .session_start(&vm, spec("python3", &["-u", "-i", "-q"]))
        .await
        .unwrap();
    let start = Instant::now();
    let r1 = b
        .session_send(&vm, &sid, b"x = 42\n", b">>> ", Duration::from_secs(10))
        .await
        .unwrap();
    println!("turn 1: {:.1} ms {r1:?}", ms(start.elapsed()));
    assert!(!r1.timed_out, "{r1:?}");
    // Stdout and stderr are separate pipes, so the prompt may overtake the
    // output: wait for the output itself.
    let start = Instant::now();
    let r2 = b
        .session_send(
            &vm,
            &sid,
            b"print(x * 2)\n",
            b"84\n",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    println!("turn 2: {:.1} ms {r2:?}", ms(start.elapsed()));
    assert!(!r2.timed_out, "{r2:?}");
    assert!(String::from_utf8_lossy(&r2.stdout).contains("84"), "{r2:?}");

    // Without `until` a send waits out its timeout and does not time out.
    let r3 = b
        .session_send(&vm, &sid, b"", b"", Duration::from_millis(50))
        .await
        .unwrap();
    assert!(!r3.timed_out);

    // The process exits: the send reports it.
    let r4 = b
        .session_send(&vm, &sid, b"exit(7)\n", b"never", Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(r4.exit_code, 7, "{r4:?}");
    assert!(!r4.timed_out, "{r4:?}");

    b.session_close(&vm, &sid).await.unwrap();
    let e = b
        .session_send(&vm, &sid, b"", b"", Duration::ZERO)
        .await
        .unwrap_err();
    assert!(matches!(e, BackendError::NotFound(_)), "{e:?}");

    // Destroying the VM ends its sessions.
    let sid = b.session_start(&vm, spec("cat", &[])).await.unwrap();
    b.destroy(&vm).await.unwrap();
    let e = b
        .session_send(&vm, &sid, b"x", b"x", Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(e, BackendError::NotFound(_)), "{e:?}");
    b.remove_template(&t).await.unwrap();
}

/// A send with timeout 0 and no `until`, as the service passes for
/// `timeout_ms: 0`, writes all of a large input however long the guest
/// takes to read it, and reports no timeout.
#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_session_large_input_zero_timeout() {
    const LEN: usize = 4 << 20;
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-input", &c).await;
    let vm = vm_name("live-input", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();

    let input: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    let script = format!("head -c {LEN} > /tmp/got");
    let sid = b
        .session_start(&vm, spec("sh", &["-c", &script]))
        .await
        .unwrap();
    let start = Instant::now();
    let r = b
        .session_send(&vm, &sid, &input, b"", Duration::ZERO)
        .await
        .unwrap();
    println!("{LEN} bytes, timeout 0: {:.1} ms", ms(start.elapsed()));
    assert!(!r.timed_out, "timed_out with an empty until");

    // `head` exits once it has read every byte.
    let r = b
        .session_send(&vm, &sid, b"", b"never", Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!r.timed_out && r.exit_code == 0, "{r:?}");
    let r = b.exec(&vm, spec("sha256sum", &["/tmp/got"])).await.unwrap();
    let want: String = sha2::Sha256::digest(&input)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(
        String::from_utf8_lossy(&r.stdout).starts_with(&want),
        "{r:?}"
    );
    b.session_close(&vm, &sid).await.unwrap();
    b.destroy(&vm).await.unwrap();
    b.remove_template(&t).await.unwrap();
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_restore_many() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-many", &c).await;

    let wall = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 1..=8 {
        let (b, c, t) = (Arc::clone(&b), c.clone(), t.clone());
        set.spawn(async move {
            let vm = vm_name("live-many", i);
            let start = Instant::now();
            b.spawn(&c, &vm, Some(&t)).await.unwrap();
            (vm, start.elapsed())
        });
    }
    let mut vms = Vec::new();
    let mut samples = Vec::new();
    while let Some(r) = set.join_next().await {
        let (vm, d) = r.unwrap();
        vms.push(vm);
        samples.push(d);
    }
    let wall = wall.elapsed();
    println!(
        "8 concurrent restores: p50 {:.0} ms, p90 {:.0} ms, max {:.0} ms, wall {:.0} ms",
        ms(percentile(&samples, 50.0)),
        ms(percentile(&samples, 90.0)),
        ms(percentile(&samples, 100.0)),
        ms(wall)
    );

    for vm in &vms {
        let r = b.exec(vm, spec("echo", &[vm])).await.unwrap();
        assert_eq!(r.stdout, format!("{vm}\n").as_bytes());
    }
    let mut listed = b.list(&vm_prefix("live-many")).await.unwrap();
    listed.sort();
    vms.sort();
    assert_eq!(listed, vms);
    for vm in &vms {
        b.destroy(vm).await.unwrap();
    }
    assert!(b.list(&vm_prefix("live-many")).await.unwrap().is_empty());
    b.remove_template(&t).await.unwrap();
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_timeout_and_cap() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-limits", &c).await;
    let vm = vm_name("live-limits", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();

    let mut s = spec("python3", &["-c", "while True: pass"]);
    s.timeout_ms = 500;
    let start = Instant::now();
    let r = b.exec(&vm, s).await.unwrap();
    let took = start.elapsed();
    println!("timeout 500 ms: returned after {:.0} ms: {r:?}", ms(took));
    assert!(r.timed_out, "{r:?}");
    assert_ne!(r.exit_code, 0);
    assert!(took >= Duration::from_millis(500) && took < Duration::from_secs(5));

    let start = Instant::now();
    let r = b
        .exec(&vm, spec("head", &["-c", "10485760", "/dev/zero"]))
        .await
        .unwrap();
    println!(
        "10 MiB of output: {:.0} ms, kept {} bytes",
        ms(start.elapsed()),
        r.stdout.len()
    );
    assert_eq!(r.stdout.len(), OUTPUT_CAP);
    assert_eq!(OUTPUT_CAP, 4194304);
    assert!(r.truncated);
    assert_eq!(r.exit_code, 0);
    assert!(!r.timed_out);

    // The VM is still usable.
    let r = b.exec(&vm, spec("echo", &["ok"])).await.unwrap();
    assert_eq!(r.stdout, b"ok\n");
    b.destroy(&vm).await.unwrap();
    b.remove_template(&t).await.unwrap();
}

/// Guest processes whose command line contains `needle`.
async fn guest_procs(b: &MicrosandboxBackend, vm: &str, needle: &str) -> Vec<String> {
    let script = "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline 2>/dev/null; echo; done";
    let r = b.exec(vm, spec("sh", &["-c", script])).await.unwrap();
    String::from_utf8_lossy(&r.stdout)
        .lines()
        .filter(|l| l.contains(needle) && !l.contains("/proc/"))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_cancel_and_destroy_race() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-race", &c).await;
    let vm = vm_name("live-race", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();

    // A dropped exec future kills its command.
    let fut = b.exec(&vm, spec("sleep", &["1001"]));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), fut)
            .await
            .is_err()
    );
    // A dropped session start or send leaks nothing either.
    let sid = b
        .session_start(&vm, spec("sleep", &["1002"]))
        .await
        .unwrap();
    let fut = b.session_send(&vm, &sid, b"", b"never", Duration::from_secs(60));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fut)
            .await
            .is_err()
    );
    b.session_close(&vm, &sid).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = guest_procs(&b, &vm, "sleep 100").await;
        if left.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "still running: {left:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // An exec on a VM that gets destroyed ends with NotFound.
    let racer = {
        let (b, vm) = (Arc::clone(&b), vm.clone());
        tokio::spawn(async move { b.exec(&vm, spec("sleep", &["30"])).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let start = Instant::now();
    b.destroy(&vm).await.unwrap();
    println!(
        "destroy with an exec in flight: {:.0} ms",
        ms(start.elapsed())
    );
    let e = racer.await.unwrap().unwrap_err();
    assert!(matches!(e, BackendError::NotFound(_)), "{e:?}");
    b.remove_template(&t).await.unwrap();
}

#[tokio::test]
#[ignore = "boots microVMs; needs Internet access"]
async fn live_network_isolation() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let probe = "import socket\ntry:\n  socket.create_connection(('1.1.1.1', 443), timeout=5).close(); print('NET-OK')\nexcept OSError as e:\n  print('NET-FAIL', e)";

    // Without setup the template has no network device at all.
    let c = cfg(&[]);
    let t = prepare(&b, "live-net-off", &c).await;
    let vm = vm_name("live-net-off", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();
    let r = b
        .exec(&vm, spec("sh", &["-c", "ls /sys/class/net"]))
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&r.stdout).contains("eth"), "{r:?}");
    let r = b.exec(&vm, spec("python3", &["-c", probe])).await.unwrap();
    assert!(r.stdout.starts_with(b"NET-FAIL"), "{r:?}");
    b.destroy(&vm).await.unwrap();
    b.remove_template(&t).await.unwrap();

    // A setup that needs the network gets it in the template VM only.
    let setup =
        "python3 -c \"import socket; socket.create_connection(('1.1.1.1', 443), timeout=5)\"";
    let c = cfg(&[("setup", setup)]);
    let t = prepare(&b, "live-net-setup", &c).await;
    let vm = vm_name("live-net-setup", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();
    let r = b.exec(&vm, spec("python3", &["-c", probe])).await.unwrap();
    println!("restored from a template with network: {r:?}");
    assert!(r.stdout.starts_with(b"NET-FAIL"), "{r:?}");
    let r = b
        .exec(
            &vm,
            spec(
                "python3",
                &[
                    "-c",
                    "import socket; print(socket.getaddrinfo('example.com', 443))",
                ],
            ),
        )
        .await
        .unwrap();
    assert_ne!(r.exit_code, 0, "DNS resolved: {r:?}");
    b.destroy(&vm).await.unwrap();
    b.remove_template(&t).await.unwrap();
}

/// Command lines of this host's `msb` processes that mention `needle`.
fn host_msb_procs(needle: &str) -> Vec<String> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("msb") && l.contains(needle))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_shutdown_no_orphans() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let component = "live-shutdown";
    let prefix = vm_prefix(component);
    let service = SandboxService::new(b.clone());
    let pool = service.replace_pool(component, cfg(&[("warmPoolSize", "3")]));
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (ready, reason) = pool.ready();
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "not ready: {reason}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let a = pool.acquire().await.unwrap();
    let r = b.exec(&a.instance_id, spec("echo", &["ok"])).await.unwrap();
    assert_eq!(r.stdout, b"ok\n");
    assert!(!host_msb_procs(&prefix).is_empty());

    service.shutdown().await;
    assert!(b.list(&prefix).await.unwrap().is_empty());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = host_msb_procs(&prefix);
        if left.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "orphans: {left:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    b.remove_template(&template_name(component)).await.unwrap();
}

/// Step 5 stretch: a Python interpreter started by the template's `setup`
/// (`setsid`, so not a microsandbox exec session or job), with modules
/// pre-imported, lives on in every restored VM. A shell client hands it code
/// through two FIFOs, skipping interpreter startup and imports.
#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_stretch_preimported_interpreter() {
    const SERVER: &str = r#"
import contextlib, io, os, traceback
import asyncio, decimal, email.parser, http.client, json  # the pre-imports
IN, OUT = '/run/pyd.in', '/run/pyd.out'
g = {}
for p in (IN, OUT):
    if not os.path.exists(p):
        os.mkfifo(p)
while True:
    with open(IN) as f:
        code = f.read()
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            exec(code, g)
        except Exception:
            traceback.print_exc()
    with open(OUT, 'w') as f:
        f.write(buf.getvalue())
"#;
    let _g = LOCK.lock().await;
    let b = backend().await;
    let setup = format!(
        "mkdir -p /run && cat > /tmp/pyd.py <<'EOF'\n{SERVER}\nEOF\n\
         setsid python3 -u /tmp/pyd.py </dev/null >/tmp/pyd.log 2>&1 &\n\
         i=0; while [ ! -p /run/pyd.out ]; do i=$((i+1)); [ $i -gt 200 ] && exit 1; sleep 0.05; done"
    );
    let c = cfg(&[("setup", &setup)]);
    let t = prepare(&b, "live-stretch", &c).await;
    let vms = [vm_name("live-stretch", 1), vm_name("live-stretch", 2)];
    for vm in &vms {
        b.spawn(&c, vm, Some(&t)).await.unwrap();
    }
    let client = |code: &str| {
        spec(
            "sh",
            &[
                "-c",
                "printf '%s' \"$1\" > /run/pyd.in && cat /run/pyd.out",
                "sh",
                code,
            ],
        )
    };

    // The daemon survived snapshot and restore, in each VM separately.
    for (vm, n) in vms.iter().zip([1, 2]) {
        let r = b.exec(vm, client(&format!("x = {n}"))).await.unwrap();
        assert_eq!(r.exit_code, 0, "{r:?}");
        let r = b
            .exec(
                vm,
                client("import sys; print(x * 10, 'http.client' in sys.modules)"),
            )
            .await
            .unwrap();
        assert_eq!(r.stdout, format!("{} True\n", n * 10).as_bytes(), "{r:?}");
    }

    let vm = &vms[0];
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for _ in 0..10 {
        let start = Instant::now();
        let r = b
            .exec(
                vm,
                spec(
                    "python3",
                    &[
                        "-c",
                        "import asyncio, decimal, email.parser, http.client, json; print(1)",
                    ],
                ),
            )
            .await
            .unwrap();
        cold.push(start.elapsed());
        assert_eq!(r.stdout, b"1\n");
        let start = Instant::now();
        let r = b.exec(vm, client("print(1)")).await.unwrap();
        warm.push(start.elapsed());
        assert_eq!(r.stdout, b"1\n");
    }
    println!(
        "python3 -c with imports: p50 {:.1} ms; pre-imported daemon: p50 {:.1} ms",
        ms(percentile(&cold, 50.0)),
        ms(percentile(&warm, 50.0))
    );
    for vm in &vms {
        b.destroy(vm).await.unwrap();
    }
    b.remove_template(&t).await.unwrap();
}

/// Waits until `pool` is ready (its warm pool full), at most 120 s.
async fn wait_full(pool: &Pool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (ready, reason) = pool.ready();
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "not ready: {reason}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Task 15a: the pool's warm-up on real VMs, with the default
/// `warmupCommand` (`true`) and with `warmupCommand: ""`. With it, every VM
/// the pool spawns runs it once, successfully, before it joins the warm
/// pool; without it, none does. Prints, for both, the first exec on a VM the
/// pool hands out (each acquire waits for a full pool, so refills do not
/// overlap it), later execs, the warm-up's cost and the time to refill 8
/// VMs.
///
/// It does not assert that the first exec is close to later ones, because
/// on microsandbox 0.7.11 it is not. An exec on a VM that has been idle for
/// more than ~25 ms costs ~28 ms on this Mac, however many execs ran before
/// (see `live_exec_latency_vs_idle`). A pooled VM always idles longer than
/// that before its handout, so the warm-up does not show in the first exec
/// (results/mac/notes.md, "Task 15a").
#[tokio::test]
#[ignore = "boots microVMs"]
async fn live_pool_warmup() {
    const N: usize = 10;
    const STEADY: usize = 3;
    const SIZE: usize = 8;
    let _g = LOCK.lock().await;
    let b = backend().await;
    for (component, warmup) in [("live-warmup-on", None), ("live-warmup-off", Some(""))] {
        let size = SIZE.to_string();
        let mut kv = vec![("warmPoolSize", size.as_str()), ("refillConcurrency", "4")];
        if let Some(v) = warmup {
            kv.push(("warmupCommand", v));
        }
        let pool = Pool::new(component, cfg(&kv), b.clone());
        pool.start();

        let (mut first, mut steady) = (Vec::new(), Vec::new());
        for _ in 0..N {
            wait_full(&pool).await;
            let a = pool.acquire().await.unwrap();
            assert_eq!(a.path, AcquirePath::Pool);
            let start = Instant::now();
            let r = b.exec(&a.instance_id, spec("echo", &["ok"])).await.unwrap();
            first.push(start.elapsed());
            assert_eq!(r.stdout, b"ok\n");
            for _ in 0..STEADY {
                let start = Instant::now();
                let r = b.exec(&a.instance_id, spec("echo", &["ok"])).await.unwrap();
                steady.push(start.elapsed());
                assert_eq!(r.stdout, b"ok\n");
            }
            pool.release(&a.instance_id).unwrap();
        }

        // Empty the pool at once and time its refill.
        wait_full(&pool).await;
        let mut taken = Vec::new();
        for _ in 0..SIZE {
            let a = pool.acquire().await.unwrap();
            assert_eq!(a.path, AcquirePath::Pool);
            taken.push(a.instance_id);
        }
        let start = Instant::now();
        wait_full(&pool).await;
        let refill = start.elapsed();
        for id in &taken {
            pool.release(id).unwrap();
        }

        let s = pool.stats();
        pool.shutdown().await;
        assert!(b.list(&vm_prefix(component)).await.unwrap().is_empty());
        b.remove_template(&template_name(component)).await.unwrap();

        let label = if s.warmups > 0 {
            format!(
                "{} warm-ups, mean {:.1} ms, max {:.1} ms",
                s.warmups,
                ms(s.warmup_total) / s.warmups as f64,
                ms(s.warmup_max)
            )
        } else {
            "no warm-up".to_string()
        };
        println!(
            "{component} ({label}): first exec after handout p50 {:.1} / p90 {:.1} / max {:.1} ms \
             (n = {N}); later execs p50 {:.1} / p90 {:.1} ms; refilling {SIZE} VMs \
             (refillConcurrency 4) took {:.0} ms",
            ms(percentile(&first, 50.0)),
            ms(percentile(&first, 90.0)),
            ms(percentile(&first, 100.0)),
            ms(percentile(&steady, 50.0)),
            ms(percentile(&steady, 90.0)),
            ms(refill),
        );
        assert_eq!(s.warmup_failures, 0, "{s:?}");
        // The initial fill, the refill after each of the N acquires (each
        // waited for a full pool), and the final refill of SIZE.
        let spawned = (SIZE + N + SIZE) as u64;
        let want = if warmup.is_none() { spawned } else { 0 };
        assert_eq!(s.warmups, want, "{s:?}");
    }
}

/// Task 15a: exec latency against how long the VM was idle before the
/// exec, on one restored VM after a warm-up exec. Prints a table; asserts
/// only that the execs work. On this Mac (microsandbox 0.7.11, HVF), up to
/// ~20 ms of idle costs ~1–2 ms; from ~25–30 ms on, ~28 ms, flat up to
/// seconds.
#[tokio::test]
#[ignore = "boots microVMs; prints timings"]
async fn live_exec_latency_vs_idle() {
    const SAMPLES: usize = 6;
    let _g = LOCK.lock().await;
    let b = backend().await;
    let c = cfg(&[]);
    let t = prepare(&b, "live-idle", &c).await;
    let vm = vm_name("live-idle", 1);
    b.spawn(&c, &vm, Some(&t)).await.unwrap();
    let start = Instant::now();
    let r = b.exec(&vm, spec("true", &[])).await.unwrap();
    assert_eq!(r.exit_code, 0);
    println!(
        "first exec (true) after restore: {:.1} ms",
        ms(start.elapsed())
    );
    for idle_ms in [0u64, 5, 10, 15, 20, 25, 30, 50, 100, 500, 2000] {
        let mut s = Vec::new();
        for _ in 0..SAMPLES {
            tokio::time::sleep(Duration::from_millis(idle_ms)).await;
            let start = Instant::now();
            let r = b.exec(&vm, spec("echo", &["ok"])).await.unwrap();
            s.push(start.elapsed());
            assert_eq!(r.stdout, b"ok\n");
        }
        let all: Vec<String> = s.iter().map(|d| format!("{:.1}", ms(*d))).collect();
        println!(
            "idle {idle_ms:4} ms, then exec echo ok: p50 {:5.1} ms  [{}]",
            ms(percentile(&s, 50.0)),
            all.join(" ")
        );
    }
    b.destroy(&vm).await.unwrap();
    b.remove_template(&t).await.unwrap();
}
