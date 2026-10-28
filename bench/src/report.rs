//! `report`: reads the run CSVs, writes `summary.md` and `rate-sweep.svg`, and
//! evaluates the gate.

use crate::stats::percentile;
use anyhow::{Context, Result, bail};
use plotters::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub const GATE_US: u64 = 100_000;
pub const GATE_RATE: u64 = 20;
pub const CSV_HEADER: &str = "path,workload,mode,rate,seq,status,acquire_path,total_us,host_us,activate_us,exec_us,client_us";

/// One CSV row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: String,
    pub workload: String,
    pub mode: String,
    pub rate: u64,
    pub seq: u64,
    pub status: u16,
    pub acquire_path: String,
    pub total_us: u64,
    pub host_us: u64,
    pub activate_us: u64,
    pub exec_us: u64,
    pub client_us: u64,
}

impl Row {
    pub fn to_csv(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            self.path,
            self.workload,
            self.mode,
            self.rate,
            self.seq,
            self.status,
            self.acquire_path,
            self.total_us,
            self.host_us,
            self.activate_us,
            self.exec_us,
            self.client_us
        )
    }

    pub fn parse(line: &str) -> Result<Row> {
        let f: Vec<&str> = line.trim().split(',').collect();
        if f.len() != 12 {
            bail!("want 12 fields, got {}: {line:?}", f.len());
        }
        Ok(Row {
            path: f[0].into(),
            workload: f[1].into(),
            mode: f[2].into(),
            rate: f[3].parse()?,
            seq: f[4].parse()?,
            status: f[5].parse()?,
            acquire_path: f[6].into(),
            total_us: f[7].parse()?,
            host_us: f[8].parse()?,
            activate_us: f[9].parse()?,
            exec_us: f[10].parse()?,
            client_us: f[11].parse()?,
        })
    }
}

/// What `generate` found.
#[derive(Debug)]
pub struct Report {
    pub pass: bool,
    /// `GATE PASS ...` or `GATE FAIL ...`, one line.
    pub verdict: String,
    pub warnings: Vec<String>,
}

/// The sidecar `run` writes next to each CSV (`<csv>.meta.json`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Meta {
    pub mode: String,
    pub rate: u64,
    pub host_pods: u64,
    pub duration_s: u64,
    pub expected: u64,
    pub fired: u64,
    pub late: u64,
    pub max_late_ms: u64,
    pub request_timeout_ms: u64,
    /// Requests still in flight at the end that were aborted (status-0 rows).
    pub aborted_requests: u64,
    /// Cleanup deletes still running at the end that were aborted.
    pub aborted_cleanups: u64,
    /// Timing values that could not be decoded (counted as 0).
    pub malformed_timing: u64,
}

pub fn meta_path(csv: &Path) -> PathBuf {
    let mut p = csv.as_os_str().to_owned();
    p.push(".meta.json");
    PathBuf::from(p)
}

/// One CSV that was read.
#[derive(Debug)]
pub struct FileInfo {
    pub name: String,
    pub rows: usize,
    /// Has rate-mode rows.
    pub has_rate: bool,
    /// Has path (a) echo rate=20 rate-mode rows (the gate's rate set).
    pub gate_rate: bool,
    pub meta: Option<Meta>,
}

pub struct Loaded {
    pub rows: Vec<Row>,
    pub files: Vec<FileInfo>,
}

pub fn load(dir: &Path) -> Result<Loaded> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "csv"))
        .collect();
    files.sort();
    let mut out = Loaded {
        rows: Vec::new(),
        files: Vec::new(),
    };
    for f in files {
        let text = std::fs::read_to_string(&f)?;
        let before = out.rows.len();
        for (i, line) in text.lines().enumerate() {
            if i == 0 && line.starts_with("path,") || line.trim().is_empty() {
                continue;
            }
            out.rows
                .push(Row::parse(line).with_context(|| format!("{}:{}", f.display(), i + 1))?);
        }
        let mine = &out.rows[before..];
        let mp = meta_path(&f);
        let meta = if mp.exists() {
            let t = std::fs::read_to_string(&mp)?;
            Some(serde_json::from_str(&t).with_context(|| format!("parsing {}", mp.display()))?)
        } else {
            None
        };
        out.files.push(FileInfo {
            name: f
                .file_name()
                .map(|n| n.to_string_lossy().into())
                .unwrap_or_default(),
            rows: mine.len(),
            has_rate: mine.iter().any(|r| r.mode == "rate"),
            gate_rate: mine
                .iter()
                .any(|r| is_gate_set(r) && r.mode == "rate" && r.rate == GATE_RATE),
            meta,
        });
    }
    Ok(out)
}

pub fn read_dir(dir: &Path) -> Result<Vec<Row>> {
    Ok(load(dir)?.rows)
}

/// How the numbers are measured; printed in summary.md and `--help`.
pub const MEASUREMENT_BASIS: &str = "\
The gate metric is the server-side timing.totalUs, measured by the caller's daprd.
client_us is the harness's wall clock around the HTTP request. It starts at the actual
send time, not at the scheduled tick time, so client-side queueing (an overloaded
harness) does not show up in either figure. It shows up only as late ticks, which is
why the gate also fails on more than 1% late ticks in rate mode.
A 200 response with timedOut=true is recorded as status 504; a transport error or
request timeout is recorded as status 0. Both count as errors in the gate.";

fn sorted(rows: &[&Row], f: impl Fn(&Row) -> u64) -> Vec<u64> {
    let mut v: Vec<u64> = rows.iter().map(|r| f(r)).collect();
    v.sort_unstable();
    v
}

fn ms(us: u64) -> String {
    format!("{:.2}", us as f64 / 1000.0)
}

fn is_gate_set(r: &Row) -> bool {
    r.path == "a" && r.workload == "echo"
}

fn is_gate_row(r: &Row) -> bool {
    is_gate_set(r) && r.status == 200 && r.acquire_path == "POOL"
}

pub const MIN_GATE_ROWS: usize = 100;

/// One gate set (sequential, or rate=20).
struct SetStats {
    name: &'static str,
    requests: usize,
    errors: usize,
    pool_ok: usize,
    nonpool: usize,
    p99: Option<u64>,
}

impl SetStats {
    fn of(name: &'static str, rows: &[Row], sel: impl Fn(&Row) -> bool) -> SetStats {
        let set: Vec<&Row> = rows.iter().filter(|r| is_gate_set(r) && sel(r)).collect();
        let errors = set.iter().filter(|r| r.status != 200).count();
        let pool: Vec<&Row> = set.iter().copied().filter(|r| is_gate_row(r)).collect();
        let ok200 = set.len() - errors;
        SetStats {
            name,
            requests: set.len(),
            errors,
            pool_ok: pool.len(),
            nonpool: ok200 - pool.len(),
            p99: (!pool.is_empty()).then(|| percentile(&sorted(&pool, |r| r.total_us), 99.0)),
        }
    }

    fn pct(a: usize, b: usize) -> f64 {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    }

    fn err_pct(&self) -> f64 {
        Self::pct(self.errors, self.requests)
    }

    fn line(&self, late: Option<Option<f64>>) -> String {
        if self.requests == 0 {
            return format!("{} n=0", self.name);
        }
        let mut s = format!(
            "{} n={} p99={}ms err={:.1}% nonpool={:.1}%",
            self.name,
            self.pool_ok,
            self.p99.map_or("-".into(), ms),
            self.err_pct(),
            Self::pct(self.nonpool, self.requests - self.errors),
        );
        match late {
            Some(Some(l)) => s.push_str(&format!(" late={l:.1}%")),
            Some(None) => s.push_str(" late=unknown"),
            None => {}
        }
        s
    }

    /// Reasons this set fails the gate.
    fn reasons(&self, late: Option<f64>) -> Vec<String> {
        let mut r = Vec::new();
        if let Some(p) = self.p99
            && p >= GATE_US
        {
            r.push(format!("{} p99 {}ms >= 100ms", self.name, ms(p)));
        }
        if self.errors * 100 > self.requests {
            r.push(format!(
                "{} err {:.1}% > 1% (non-200 incl. status 0)",
                self.name,
                self.err_pct()
            ));
        }
        if self.pool_ok < MIN_GATE_ROWS {
            r.push(format!(
                "{} fewer than {MIN_GATE_ROWS} POOL rows ({})",
                self.name, self.pool_ok
            ));
        }
        if let Some(l) = late
            && l > 1.0
        {
            r.push(format!("{} late {l:.1}% > 1%", self.name));
        }
        r
    }
}

pub fn generate(in_dir: &Path, out_dir: &Path) -> Result<Report> {
    let loaded = load(in_dir)?;
    let rows = loaded.rows;
    std::fs::create_dir_all(out_dir)?;
    let mut warnings = Vec::new();

    // Late ticks of the rate=20 set, from the sidecars; None when unknown.
    let rate_files: Vec<&FileInfo> = loaded.files.iter().filter(|f| f.gate_rate).collect();
    for f in loaded
        .files
        .iter()
        .filter(|f| f.has_rate && f.meta.is_none())
    {
        warnings.push(format!(
            "WARNING: {} has no sidecar {}.meta.json; late ticks are unknown",
            f.name, f.name
        ));
    }
    for f in &loaded.files {
        if let Some(m) = &f.meta {
            if m.malformed_timing > 0 {
                warnings.push(format!(
                    "WARNING: {}: {} malformed timing values (counted as 0)",
                    f.name, m.malformed_timing
                ));
            }
            if m.aborted_requests > 0 {
                warnings.push(format!(
                    "WARNING: {}: {} requests aborted at the end of the run",
                    f.name, m.aborted_requests
                ));
            }
        }
    }
    let late_pct = if rate_files.iter().all(|f| f.meta.is_some()) {
        let fired: u64 = rate_files
            .iter()
            .filter_map(|f| f.meta.as_ref())
            .map(|m| m.fired)
            .sum();
        let late: u64 = rate_files
            .iter()
            .filter_map(|f| f.meta.as_ref())
            .map(|m| m.late)
            .sum();
        Some(if fired == 0 {
            0.0
        } else {
            100.0 * late as f64 / fired as f64
        })
    } else {
        None
    };

    let seq = SetStats::of("sequential", &rows, |r| r.mode == "sequential");
    let rate = SetStats::of("rate20", &rows, |r| r.mode == "rate" && r.rate == GATE_RATE);
    let mut reasons = Vec::new();
    if seq.requests == 0 {
        reasons.push("no sequential rows".to_string());
    } else {
        reasons.extend(seq.reasons(None));
    }
    if rate.requests == 0 {
        reasons.push(format!("no rate={GATE_RATE} rows"));
    } else {
        reasons.extend(rate.reasons(late_pct));
    }
    let pass = reasons.is_empty();
    let stats = format!("{}; {}", seq.line(None), rate.line(Some(late_pct)));
    let verdict = if pass {
        format!("GATE PASS ({stats})")
    } else {
        format!("GATE FAIL ({stats}): {}", reasons.join("; "))
    };

    // Path (a) accounting.
    let a_ok: Vec<&Row> = rows
        .iter()
        .filter(|r| r.path == "a" && r.workload == "echo" && r.status == 200)
        .collect();
    let not_pool = a_ok.iter().filter(|r| r.acquire_path != "POOL").count();
    if !a_ok.is_empty() && not_pool * 100 > a_ok.len() {
        warnings.push(format!(
            "WARNING: {:.1}% of path (a) echo rows ({not_pool} of {}) have acquire_path != POOL; they are excluded from the gate",
            100.0 * not_pool as f64 / a_ok.len() as f64,
            a_ok.len()
        ));
    }

    // summary.md
    let mut s = String::new();
    writeln!(s, "# Sandbox latency summary\n")?;
    writeln!(s, "{verdict}\n")?;
    writeln!(s, "## Measurement basis\n\n{MEASUREMENT_BASIS}\n")?;
    writeln!(s, "## Input files\n")?;
    for f in &loaded.files {
        write!(s, "- {}: {} rows", f.name, f.rows)?;
        match &f.meta {
            Some(m) => {
                let pct = if m.fired == 0 {
                    0.0
                } else {
                    100.0 * m.late as f64 / m.fired as f64
                };
                write!(
                    s,
                    "; {} rate {} x {} hosts: expected {}, fired {}, late {} ({pct:.1}%), max lateness {} ms, request timeout {} ms, aborted requests {}, aborted cleanups {}, malformed timing values {}",
                    m.mode,
                    m.rate,
                    m.host_pods,
                    m.expected,
                    m.fired,
                    m.late,
                    m.max_late_ms,
                    m.request_timeout_ms,
                    m.aborted_requests,
                    m.aborted_cleanups,
                    m.malformed_timing
                )?;
            }
            None => write!(s, "; no sidecar")?,
        }
        writeln!(s)?;
    }
    writeln!(s)?;
    for w in &warnings {
        writeln!(s, "{w}\n")?;
    }
    let non200 = rows.iter().filter(|r| r.status != 200).count();
    writeln!(s, "- rows: {}", rows.len())?;
    writeln!(
        s,
        "- non-200 rows: {non200} (excluded from percentiles, never dropped)"
    )?;
    let mut by_status: BTreeMap<u16, usize> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.status != 200) {
        *by_status.entry(r.status).or_default() += 1;
    }
    for (st, n) in &by_status {
        writeln!(s, "  - HTTP {st}: {n}")?;
    }
    writeln!(
        s,
        "- path (a) requests with acquire_path != POOL: {not_pool}\n"
    )?;

    type Key = (String, String, String, u64);
    let mut groups: BTreeMap<Key, Vec<&Row>> = BTreeMap::new();
    for r in &rows {
        groups
            .entry((r.path.clone(), r.workload.clone(), r.mode.clone(), r.rate))
            .or_default()
            .push(r);
    }
    writeln!(s, "## Latency (HTTP 200 rows, ms)\n")?;
    writeln!(
        s,
        "| path | workload | mode | rate | n | non-200 | acquire paths | total p50 | total p99 | client p50 | client p99 |"
    )?;
    writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|")?;
    let mut breakdown = String::new();
    writeln!(breakdown, "\n## Breakdown (HTTP 200 rows, median ms)\n")?;
    writeln!(
        breakdown,
        "| path | workload | mode | rate | host | activate | exec | host other (host - activate - exec) | daprd (total - host) |"
    )?;
    writeln!(breakdown, "|---|---|---|---|---|---|---|---|---|")?;
    for ((p, w, m, rt), g) in &groups {
        let ok: Vec<&Row> = g.iter().copied().filter(|r| r.status == 200).collect();
        let bad = g.len() - ok.len();
        let mut paths: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &ok {
            *paths
                .entry(if r.acquire_path.is_empty() {
                    "-"
                } else {
                    &r.acquire_path
                })
                .or_default() += 1;
        }
        let paths = paths
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        if ok.is_empty() {
            writeln!(
                s,
                "| {p} | {w} | {m} | {rt} | 0 | {bad} | - | - | - | - | - |"
            )?;
            continue;
        }
        let t = sorted(&ok, |r| r.total_us);
        let c = sorted(&ok, |r| r.client_us);
        writeln!(
            s,
            "| {p} | {w} | {m} | {rt} | {} | {bad} | {paths} | {} | {} | {} | {} |",
            ok.len(),
            ms(percentile(&t, 50.0)),
            ms(percentile(&t, 99.0)),
            ms(percentile(&c, 50.0)),
            ms(percentile(&c, 99.0)),
        )?;
        let med = |f: fn(&Row) -> u64| percentile(&sorted(&ok, f), 50.0);
        let (h, a, e, tt) = (
            med(|r| r.host_us),
            med(|r| r.activate_us),
            med(|r| r.exec_us),
            med(|r| r.total_us),
        );
        writeln!(
            breakdown,
            "| {p} | {w} | {m} | {rt} | {} | {} | {} | {} | {} |",
            ms(h),
            ms(a),
            ms(e),
            ms(h.saturating_sub(a + e)),
            ms(tt.saturating_sub(h))
        )?;
    }
    s.push_str(&breakdown);
    std::fs::write(out_dir.join("summary.md"), s)?;

    plot(&rows, &out_dir.join("rate-sweep.svg"))?;
    Ok(Report {
        pass,
        verdict,
        warnings,
    })
}

/// p50/p99 of gate rows (path a, echo, POOL, 200) against the rate.
fn plot(rows: &[Row], file: &Path) -> Result<()> {
    let mut by_rate: BTreeMap<u64, Vec<&Row>> = BTreeMap::new();
    for r in rows.iter().filter(|r| is_gate_row(r) && r.mode == "rate") {
        by_rate.entry(r.rate).or_default().push(r);
    }
    let (mut p50, mut p99) = (Vec::new(), Vec::new());
    for (rate, g) in &by_rate {
        let v = sorted(g, |r| r.total_us);
        p50.push((*rate as f64, percentile(&v, 50.0) as f64 / 1000.0));
        p99.push((*rate as f64, percentile(&v, 99.0) as f64 / 1000.0));
    }
    let max_x = p99.iter().map(|p| p.0).fold(1.0, f64::max) * 1.05;
    let max_y = p99.iter().map(|p| p.1).fold(100.0, f64::max) * 1.1;
    let root = SVGBackend::new(file, (800, 480)).into_drawing_area();
    root.fill(&WHITE)?;
    let mut chart = ChartBuilder::on(&root)
        .margin(20)
        .x_label_area_size(40)
        .y_label_area_size(60)
        .build_cartesian_2d(0.0..max_x, 0.0..max_y)?;
    chart
        .configure_mesh()
        .x_desc("rate (requests/s)")
        .y_desc("total latency (ms)")
        .draw()?;
    chart
        .draw_series(LineSeries::new(p50.iter().copied(), BLUE.stroke_width(2)))?
        .label("p50")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], BLUE));
    chart
        .draw_series(LineSeries::new(p99.iter().copied(), RED.stroke_width(2)))?
        .label("p99")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], RED));
    chart.draw_series(LineSeries::new(
        [(0.0, 100.0), (max_x, 100.0)],
        BLACK.mix(0.4),
    ))?;
    chart.configure_series_labels().border_style(BLACK).draw()?;
    root.present()?;
    Ok(())
}
