//! The driver's `--selftest IMAGE`: one template, one VM restored from it,
//! `echo ok`, and the timings of each step, printed to stdout. Also home to
//! the small timing helpers the live tests share.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::backend::{ExecSpec, VmBackend};
use crate::config::Config;
use crate::microsandbox_backend::MicrosandboxBackend;
use crate::pool::{template_name, vm_name, vm_prefix};

/// The component name the selftest's VMs and template derive from.
const COMPONENT: &str = "dapr-sandbox-microvm-selftest";
const RUNS: usize = 20;

/// `d` in milliseconds.
pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The `p`-th percentile (nearest rank) of `samples`, which must not be
/// empty.
pub fn percentile(samples: &[Duration], p: f64) -> Duration {
    let mut s = samples.to_vec();
    s.sort();
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

fn echo_ok() -> ExecSpec {
    ExecSpec {
        command: "echo".into(),
        args: vec!["ok".into()],
        ..Default::default()
    }
}

/// Runs the selftest in microsandbox home `msb_home`, printing each step;
/// returns whether all passed. Cleans up its VMs and template either way.
pub async fn run(image: &str, msb_home: PathBuf) -> bool {
    println!(
        "selftest: image {image}, microsandbox home {}",
        msb_home.display()
    );
    let start = Instant::now();
    let b = match MicrosandboxBackend::new(msb_home).await {
        Ok(b) => b,
        Err(e) => {
            println!("selftest: FAILED: {e}");
            return false;
        }
    };
    println!("  backend init       {:9.1} ms", ms(start.elapsed()));
    let template = template_name(COMPONENT);
    let vm = vm_name(COMPONENT, 1);
    let result = steps(&b, image, &template, &vm).await;
    // Clean up whatever exists, also after a failure.
    if let Ok(left) = b.list(&vm_prefix(COMPONENT)).await {
        for name in left {
            let _ = b.destroy(&name).await;
        }
    }
    if let Err(e) = b.remove_template(&template).await {
        println!("  (removing the template: {e})");
    }
    println!("  total              {:9.1} ms", ms(start.elapsed()));
    match result {
        Ok(()) => {
            println!("selftest: ok");
            true
        }
        Err(e) => {
            println!("selftest: FAILED: {e}");
            false
        }
    }
}

async fn steps(
    b: &MicrosandboxBackend,
    image: &str,
    template: &str,
    vm: &str,
) -> Result<(), String> {
    // Leftovers of an interrupted selftest.
    for name in b
        .list(&vm_prefix(COMPONENT))
        .await
        .map_err(|e| e.to_string())?
    {
        b.destroy(&name).await.map_err(|e| e.to_string())?;
    }
    let cfg = Config::from_properties(&HashMap::from([("image".into(), image.to_string())]))
        .map_err(|e| e.to_string())?;

    let t = Instant::now();
    b.prepare_template(&cfg, template)
        .await
        .map_err(|e| format!("preparing the template: {e}"))?;
    let prepare = t.elapsed();
    match b.last_prepare_timings() {
        Some(p) => println!(
            "  template prepare   {:9.1} ms  (boot {:.1}, snapshot {:.1}, destroy {:.1} ms; \
             snapshot {} MiB)",
            ms(prepare),
            ms(p.boot),
            ms(p.snapshot),
            ms(p.destroy),
            p.snapshot_bytes
                .map_or_else(|| "?".to_string(), |n| (n >> 20).to_string())
        ),
        None => println!("  template prepare   {:9.1} ms", ms(prepare)),
    }

    let t = Instant::now();
    b.spawn(&cfg, vm, Some(template))
        .await
        .map_err(|e| format!("restoring a VM: {e}"))?;
    println!("  restore (spawn)    {:9.1} ms", ms(t.elapsed()));

    let t = Instant::now();
    let r = b
        .exec(vm, echo_ok())
        .await
        .map_err(|e| format!("exec: {e}"))?;
    let first = t.elapsed();
    if r.stdout != b"ok\n" || r.exit_code != 0 {
        return Err(format!("exec echo ok: unexpected result {r:?}"));
    }
    println!(
        "  first exec         {:9.1} ms  (stdout {:?})",
        ms(first),
        String::from_utf8_lossy(&r.stdout)
    );

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        let r = b
            .exec(vm, echo_ok())
            .await
            .map_err(|e| format!("exec: {e}"))?;
        samples.push(t.elapsed());
        if r.stdout != b"ok\n" {
            return Err(format!("exec echo ok: unexpected result {r:?}"));
        }
    }
    println!(
        "  exec echo ok x{RUNS}   p50 {:.2} ms, p90 {:.2} ms, max {:.2} ms",
        ms(percentile(&samples, 50.0)),
        ms(percentile(&samples, 90.0)),
        ms(percentile(&samples, 100.0))
    );

    let t = Instant::now();
    b.destroy(vm).await.map_err(|e| format!("destroy: {e}"))?;
    println!("  destroy            {:9.1} ms", ms(t.elapsed()));
    Ok(())
}
