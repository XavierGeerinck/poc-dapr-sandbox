//! `report`: reads the run CSVs, writes `summary.md` and `rate-sweep.svg`, and
//! evaluates the gate.

use crate::stats::percentile;
use anyhow::{Context, Result, bail};
use plotters::prelude::*;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

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

pub fn read_dir(dir: &Path) -> Result<Vec<Row>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "csv"))
        .collect();
    files.sort();
    let mut rows = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f)?;
        for (i, line) in text.lines().enumerate() {
            if i == 0 && line.starts_with("path,") || line.trim().is_empty() {
                continue;
            }
            rows.push(Row::parse(line).with_context(|| format!("{}:{}", f.display(), i + 1))?);
        }
    }
    Ok(rows)
}

fn sorted(rows: &[&Row], f: impl Fn(&Row) -> u64) -> Vec<u64> {
    let mut v: Vec<u64> = rows.iter().map(|r| f(r)).collect();
    v.sort_unstable();
    v
}

fn ms(us: u64) -> String {
    format!("{:.2}", us as f64 / 1000.0)
}

fn is_gate_row(r: &Row) -> bool {
    r.path == "a" && r.workload == "echo" && r.status == 200 && r.acquire_path == "POOL"
}

/// p99 of the gate rows selected by `sel`, or `None` if there are none.
fn gate_p99(rows: &[Row], sel: impl Fn(&Row) -> bool) -> Option<(usize, u64)> {
    let g: Vec<&Row> = rows.iter().filter(|r| is_gate_row(r) && sel(r)).collect();
    if g.is_empty() {
        return None;
    }
    Some((g.len(), percentile(&sorted(&g, |r| r.total_us), 99.0)))
}

pub fn generate(in_dir: &Path, out_dir: &Path) -> Result<Report> {
    let rows = read_dir(in_dir)?;
    std::fs::create_dir_all(out_dir)?;
    let mut warnings = Vec::new();

    // Gate.
    let seq = gate_p99(&rows, |r| r.mode == "sequential");
    let rate = gate_p99(&rows, |r| r.mode == "rate" && r.rate == GATE_RATE);
    let mut reasons = Vec::new();
    let mut detail = Vec::new();
    match seq {
        None => reasons.push("no sequential rows".to_string()),
        Some((n, p)) => {
            detail.push(format!("sequential p99={}ms (n={n})", ms(p)));
            if p >= GATE_US {
                reasons.push(format!("sequential p99 {}ms >= 100ms", ms(p)));
            }
        }
    }
    match rate {
        None => reasons.push(format!("no rate={GATE_RATE} rows")),
        Some((n, p)) => {
            detail.push(format!("rate={GATE_RATE} p99={}ms (n={n})", ms(p)));
            if p >= GATE_US {
                reasons.push(format!("rate={GATE_RATE} p99 {}ms >= 100ms", ms(p)));
            }
        }
    }
    let pass = reasons.is_empty();
    let verdict = if pass {
        format!("GATE PASS: {}", detail.join(", "))
    } else {
        format!("GATE FAIL: {}", reasons.join("; "))
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
