//! `run`: the request loops (sequential and open-loop rate) against daprd's
//! alpha Sandbox HTTP API.

use crate::report::{CSV_HEADER, Meta, Row, meta_path};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    Echo,
    Python,
    SessionImport,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Workload::Echo => "echo",
            Workload::Python => "python",
            Workload::SessionImport => "session-import",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub dapr_url: String,
    pub app_id: String,
    pub component: String,
    /// `a`, `b`, `c` or `d`.
    pub path: String,
    pub workload: Workload,
    /// Per HTTP request; a timeout is a status-0 row.
    pub request_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Sequential {
        count: u64,
    },
    /// `rate` is per host pod; the firing interval is `1 / (rate * hosts)`.
    Rate {
        rate: u64,
        hosts: u64,
        duration: Duration,
    },
}

/// Timing values that could not be decoded (counted as 0, reported).
static MALFORMED: AtomicU64 = AtomicU64::new(0);

/// Malformed timing values seen since the run started.
pub fn malformed_count() -> u64 {
    MALFORMED.load(Ordering::Relaxed)
}

/// protojson renders uint64 as a JSON string; accept a number too. Anything
/// else is counted in `MALFORMED` and read as 0.
fn u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = match Value::deserialize(d)? {
        Value::Null => Some(0),
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    };
    Ok(v.unwrap_or_else(|| {
        MALFORMED.fetch_add(1, Ordering::Relaxed);
        0
    }))
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Timing {
    #[serde(deserialize_with = "u64_lenient")]
    pub total_us: u64,
    #[serde(deserialize_with = "u64_lenient")]
    pub host_us: u64,
    #[serde(deserialize_with = "u64_lenient")]
    pub activate_us: u64,
    #[serde(deserialize_with = "u64_lenient")]
    pub exec_us: u64,
    pub acquire_path: String,
}

/// An exec-shaped (or session-start) response; every field may be absent.
#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Resp {
    pub session_id: String,
    pub activation: String,
    pub timed_out: bool,
    pub timing: Option<Timing>,
}

/// One timed HTTP call: status, decoded body, wall clock around it.
struct Call {
    status: u16,
    resp: Resp,
    client_us: u64,
}

struct Ctx {
    http: reqwest::Client,
    cfg: Config,
    /// Cleanups (deletes) running off the measured path.
    cleanups: Mutex<Vec<JoinHandle<()>>>,
    /// The warmed sandbox of path b.
    fixed_id: Option<String>,
    /// The CSV, appended and flushed per row so a killed run keeps its rows.
    csv: Mutex<std::fs::File>,
    timeout: Duration,
    aborted_requests: AtomicU64,
    aborted_cleanups: AtomicU64,
}

impl Ctx {
    fn url(&self, id: &str, rest: &str) -> String {
        format!(
            "{}/v1.0-alpha1/sandboxes/{}/{id}{rest}",
            self.cfg.dapr_url.trim_end_matches('/'),
            self.cfg.component
        )
    }

    async fn post(&self, id: &str, rest: &str, body: Value) -> Result<Call> {
        let start = Instant::now();
        let res = self
            .http
            .post(self.url(id, rest))
            .header("dapr-app-id", &self.cfg.app_id)
            .json(&body)
            .send()
            .await?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await?;
        let client_us = start.elapsed().as_micros() as u64;
        // An error body is not an exec response: keep the status, default the rest.
        let resp = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            if status == 200 {
                MALFORMED.fetch_add(1, Ordering::Relaxed);
            }
            Resp::default()
        });
        Ok(Call {
            status,
            resp,
            client_us,
        })
    }

    /// Spawns `DELETE` calls off the measured path (and the open loop).
    fn spawn_cleanup(&self, id: String, session: Option<String>) -> usize {
        let http = self.http.clone();
        let app = self.cfg.app_id.clone();
        let sandbox = self.url(&id, "");
        let sess = session.map(|s| self.url(&id, &format!("/sessions/{s}")));
        let h = tokio::spawn(async move {
            if let Some(u) = sess {
                let _ = http.delete(u).header("dapr-app-id", &app).send().await;
            }
            let _ = http
                .delete(sandbox)
                .header("dapr-app-id", &app)
                .send()
                .await;
        });
        let mut c = self.cleanups.lock().unwrap();
        c.push(h);
        c.len() - 1
    }

    /// Waits for the cleanups, at most `timeout + 10 s`; aborts and counts the rest.
    async fn drain_cleanups(&self) {
        let hs: Vec<_> = std::mem::take(&mut *self.cleanups.lock().unwrap());
        let deadline = tokio::time::Instant::now() + self.timeout + Duration::from_secs(10);
        for mut h in hs {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
                self.aborted_cleanups.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn write_row(&self, r: &Row) {
        let mut f = self.csv.lock().unwrap();
        if let Err(e) = writeln!(f, "{}", r.to_csv()) {
            eprintln!("writing the CSV: {e}");
        }
    }

    fn exec_body(&self) -> Value {
        match self.cfg.workload {
            Workload::Python => json!({"command": "python3", "args": ["-c", "print(1)"]}),
            // The driver passes command and args as argv, not through a shell.
            _ => json!({"command": "echo", "args": ["ok"]}),
        }
    }
}

fn row(ctx: &Ctx, mode: &str, rate: u64, seq: u64, c: Option<&Call>, path: &str) -> Row {
    let t = c.and_then(|c| c.resp.timing.as_ref());
    // A 200 that timed out inside the sandbox is an error: status 504.
    let status = match c {
        Some(c) if c.status == 200 && c.resp.timed_out => 504,
        Some(c) => c.status,
        None => 0,
    };
    Row {
        path: ctx.cfg.path.clone(),
        workload: ctx.cfg.workload.name().into(),
        mode: mode.into(),
        rate,
        seq,
        status,
        acquire_path: path.into(),
        total_us: t.map_or(0, |t| t.total_us),
        host_us: t.map_or(0, |t| t.host_us),
        activate_us: t.map_or(0, |t| t.activate_us),
        exec_us: t.map_or(0, |t| t.exec_us),
        client_us: c.map_or(0, |c| c.client_us),
    }
}

fn path_of(c: &Call) -> String {
    c.resp
        .timing
        .as_ref()
        .map(|t| t.acquire_path.clone())
        .unwrap_or_default()
}

/// Runs one measured request and schedules its cleanup.
async fn one(ctx: &Ctx, mode: &str, rate: u64, seq: u64) -> Row {
    let r = match one_inner(ctx, mode, rate, seq).await {
        Ok(r) => r,
        // A transport error or timeout is a row with status 0, never an abort.
        Err(e) => {
            eprintln!("seq {seq}: {e:#}");
            row(ctx, mode, rate, seq, None, "")
        }
    };
    ctx.write_row(&r);
    r
}

async fn one_inner(ctx: &Ctx, mode: &str, rate: u64, seq: u64) -> Result<Row> {
    if ctx.cfg.workload == Workload::SessionImport {
        return session_import(ctx, mode, rate, seq).await;
    }
    let (id, fresh) = match &ctx.fixed_id {
        Some(id) => (id.clone(), false),
        None => (uuid::Uuid::new_v4().to_string(), true),
    };
    let c = ctx.post(&id, "/exec", ctx.exec_body()).await;
    if fresh {
        ctx.spawn_cleanup(id, None);
    }
    let c = c?;
    Ok(row(ctx, mode, rate, seq, Some(&c), &path_of(&c)))
}

fn b64(s: &str) -> String {
    B64.encode(s)
}

async fn session_import(ctx: &Ctx, mode: &str, rate: u64, seq: u64) -> Result<Row> {
    let id = match &ctx.fixed_id {
        Some(id) => id.clone(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let fresh = ctx.fixed_id.is_none();
    let finish = |session: Option<String>| {
        if fresh {
            ctx.spawn_cleanup(id.clone(), session);
        } else if let Some(s) = session {
            // Path b keeps its sandbox but closes the session.
            let (http, app, u) = (
                ctx.http.clone(),
                ctx.cfg.app_id.clone(),
                ctx.url(&id, &format!("/sessions/{s}")),
            );
            ctx.cleanups.lock().unwrap().push(tokio::spawn(async move {
                let _ = http.delete(u).header("dapr-app-id", &app).send().await;
            }));
        }
    };
    let start = ctx
        .post(
            &id,
            "/sessions",
            json!({"command": "python3", "args": ["-u", "-i", "-q"]}),
        )
        .await;
    let start = match start {
        Ok(s) => s,
        Err(e) => {
            finish(None);
            return Err(e);
        }
    };
    let acquire = path_of(&start);
    if start.status != 200 || start.resp.session_id.is_empty() {
        finish(None);
        return Ok(row(ctx, mode, rate, seq, Some(&start), &acquire));
    }
    let sid = start.resp.session_id.clone();
    let act = start.resp.activation.clone();
    let turn = |input: &str| json!({"input": b64(input), "until": b64(">>> "), "timeoutMs": 10000, "expectedActivation": act});
    let sess = format!("/sessions/{sid}");
    // Untimed: consume the startup prompt.
    let warm = ctx.post(&id, &sess, turn("")).await;
    if let Err(e) = warm {
        finish(Some(sid));
        return Err(e);
    }
    let timed = ctx
        .post(
            &id,
            &sess,
            turn("import json, decimal, email.parser; print(1)\n"),
        )
        .await;
    finish(Some(sid));
    let timed = timed?;
    Ok(row(ctx, mode, rate, seq, Some(&timed), &acquire))
}

pub async fn run(cfg: Config, mode: Mode, out: &Path) -> Result<Vec<Row>> {
    let timeout = Duration::from_millis(cfg.request_timeout_ms);
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .timeout(timeout)
        .build()?;
    if let Some(p) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p)?;
    }
    let mut file =
        std::fs::File::create(out).with_context(|| format!("writing {}", out.display()))?;
    writeln!(file, "{CSV_HEADER}")?;
    MALFORMED.store(0, Ordering::Relaxed);
    let calls: u32 = if cfg.workload == Workload::SessionImport {
        3
    } else {
        1
    };
    let mut ctx = Ctx {
        http,
        cfg,
        cleanups: Mutex::new(Vec::new()),
        fixed_id: None,
        csv: Mutex::new(file),
        timeout,
        aborted_requests: AtomicU64::new(0),
        aborted_cleanups: AtomicU64::new(0),
    };

    // Warm keep-alive connections untimed.
    let health = format!("{}/v1.0/healthz", ctx.cfg.dapr_url.trim_end_matches('/'));
    let n = if matches!(mode, Mode::Rate { .. }) {
        16
    } else {
        1
    };
    let pings: Vec<_> = (0..n)
        .map(|_| {
            let (h, u) = (ctx.http.clone(), health.clone());
            tokio::spawn(async move { h.get(u).send().await.map(|r| r.status()) })
        })
        .collect();
    for p in pings {
        if let Ok(Err(e)) = p.await {
            bail!("daprd not reachable at {}: {e}", ctx.cfg.dapr_url);
        }
    }

    // Path b: one warmed ID, deleted at the end.
    if ctx.cfg.path == "b" {
        let id = format!("bench-b-{}", uuid::Uuid::new_v4());
        let w = ctx
            .post(&id, "/exec", ctx.exec_body())
            .await
            .context("warming the path b sandbox")?;
        if w.status != 200 {
            bail!("warming the path b sandbox: HTTP {}", w.status);
        }
        ctx.fixed_id = Some(id);
    }
    let ctx = Arc::new(ctx);

    let mut rows: Vec<Row> = Vec::new();
    let mut meta = Meta {
        request_timeout_ms: ctx.cfg.request_timeout_ms,
        ..Meta::default()
    };
    match mode {
        Mode::Sequential { count } => {
            meta.mode = "sequential".into();
            meta.expected = count;
            for seq in 0..count {
                rows.push(one(&ctx, "sequential", 0, seq).await);
                meta.fired += 1;
                ctx.drain_cleanups().await;
            }
        }
        Mode::Rate {
            rate,
            hosts,
            duration,
        } => {
            let interval = Duration::from_secs_f64(1.0 / (rate * hosts) as f64);
            let total = (duration.as_secs_f64() * (rate * hosts) as f64).round() as u64;
            meta.mode = "rate".into();
            meta.rate = rate;
            meta.host_pods = hosts;
            meta.duration_s = duration.as_secs();
            meta.expected = total;
            let t0 = tokio::time::Instant::now();
            let mut handles = Vec::with_capacity(total as usize);
            let mut max_late = Duration::ZERO;
            for seq in 0..total {
                // Fixed schedule: a late tick fires immediately and the ones
                // after it keep their original slots, so none is dropped.
                let due = t0 + interval.mul_f64(seq as f64);
                tokio::time::sleep_until(due).await;
                let lag = due.elapsed();
                if lag > interval / 2 {
                    meta.late += 1;
                    max_late = max_late.max(lag);
                }
                let c = ctx.clone();
                handles.push((
                    seq,
                    tokio::spawn(async move { one(&c, "rate", rate, seq).await }),
                ));
                meta.fired += 1;
            }
            meta.max_late_ms = max_late.as_millis() as u64;
            // Bounded wait: every request has the client timeout, so a task
            // still running after this has hung; abort it and record status 0.
            let deadline = tokio::time::Instant::now() + timeout * calls + Duration::from_secs(10);
            for (seq, mut h) in handles {
                match tokio::time::timeout_at(deadline, &mut h).await {
                    Ok(Ok(r)) => rows.push(r),
                    failed => {
                        if failed.is_err() {
                            h.abort();
                            ctx.aborted_requests.fetch_add(1, Ordering::Relaxed);
                        }
                        eprintln!("seq {seq}: task did not finish (panicked or hung)");
                        let r = row(&ctx, "rate", rate, seq, None, "");
                        ctx.write_row(&r);
                        rows.push(r);
                    }
                }
            }
            eprintln!(
                "rate {rate}/s x {hosts} hosts: fired {} of {total} requests, {} late ticks (max lateness {} ms)",
                meta.fired, meta.late, meta.max_late_ms
            );
        }
    }

    // End of run: untimed cleanup.
    if let Some(id) = &ctx.fixed_id {
        let _ = ctx
            .http
            .delete(ctx.url(id, ""))
            .header("dapr-app-id", &ctx.cfg.app_id)
            .send()
            .await;
    }
    ctx.drain_cleanups().await;

    meta.aborted_requests = ctx.aborted_requests.load(Ordering::Relaxed);
    meta.aborted_cleanups = ctx.aborted_cleanups.load(Ordering::Relaxed);
    meta.malformed_timing = MALFORMED.load(Ordering::Relaxed);
    if meta.malformed_timing > 0 {
        eprintln!(
            "warning: {} malformed timing values (counted as 0)",
            meta.malformed_timing
        );
    }
    if meta.aborted_requests > 0 || meta.aborted_cleanups > 0 {
        eprintln!(
            "warning: aborted at the end: {} requests, {} cleanups",
            meta.aborted_requests, meta.aborted_cleanups
        );
    }
    let mp = meta_path(out);
    std::fs::write(&mp, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("writing {}", mp.display()))?;
    Ok(rows)
}
