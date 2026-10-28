use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sandbox_bench::report;
use sandbox_bench::run::{Config, Mode, Workload, run};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "sandbox-bench",
    about = "Latency harness for the Dapr sandbox building block",
    after_help = report::MEASUREMENT_BASIS
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum WorkloadArg {
    Echo,
    Python,
    SessionImport,
}

impl From<WorkloadArg> for Workload {
    fn from(w: WorkloadArg) -> Self {
        match w {
            WorkloadArg::Echo => Workload::Echo,
            WorkloadArg::Python => Workload::Python,
            WorkloadArg::SessionImport => Workload::SessionImport,
        }
    }
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum ModeArg {
    Sequential,
    Rate,
}

#[derive(clap::Args, Clone)]
struct Target {
    #[arg(long, default_value = "http://localhost:3500")]
    dapr_url: String,
    #[arg(long, default_value = "sandbox-host")]
    app_id: String,
    #[arg(long, value_enum, default_value = "echo")]
    workload: WorkloadArg,
    /// Per-request timeout; a timeout is recorded as a status-0 row.
    #[arg(long, default_value_t = 30000)]
    request_timeout_ms: u64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one measurement and write a CSV.
    Run {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value = "py-fast")]
        component: String,
        /// a: warm pool, b: reused sandbox, c: restore, d: cold boot.
        #[arg(long, value_parser = ["a", "b", "c", "d"], default_value = "a")]
        path: String,
        #[arg(long, value_enum, default_value = "sequential")]
        mode: ModeArg,
        #[arg(long, default_value_t = 20)]
        count: u64,
        /// Requests per second per host pod.
        #[arg(long, default_value_t = 20)]
        rate: u64,
        #[arg(long, default_value_t = 1)]
        host_pods: u64,
        /// Seconds, in rate mode.
        #[arg(long, default_value_t = 10)]
        duration: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Path (a) in rate mode for each rate; one CSV per rate in --out-dir.
    Sweep {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value = "py-fast")]
        component: String,
        #[arg(long, default_value = "10,20,40,80,160", value_delimiter = ',')]
        rates: Vec<u64>,
        #[arg(long, default_value_t = 1)]
        host_pods: u64,
        #[arg(long, default_value_t = 10)]
        duration: u64,
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Write summary.md and rate-sweep.svg; exit non-zero on GATE FAIL.
    Report {
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

fn path_component(p: &str, c: &str) -> String {
    // The paths are tied to their components by name unless overridden.
    match (p, c) {
        ("c", "py-fast") => "py-restore".into(),
        ("d", "py-fast") => "py-boot".into(),
        _ => c.into(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run {
            target,
            component,
            path,
            mode,
            count,
            rate,
            host_pods,
            duration,
            out,
        } => {
            if mode == ModeArg::Rate && (rate == 0 || host_pods == 0) {
                bail!("--rate and --host-pods must be positive");
            }
            let cfg = Config {
                dapr_url: target.dapr_url,
                app_id: target.app_id,
                component: path_component(&path, &component),
                path,
                workload: target.workload.into(),
                request_timeout_ms: target.request_timeout_ms,
            };
            let mode = match mode {
                ModeArg::Sequential => Mode::Sequential { count },
                ModeArg::Rate => Mode::Rate {
                    rate,
                    hosts: host_pods,
                    duration: Duration::from_secs(duration),
                },
            };
            let rows = run(cfg, mode, &out).await?;
            let bad = rows.iter().filter(|r| r.status != 200).count();
            println!(
                "{} rows written to {} ({bad} non-200)",
                rows.len(),
                out.display()
            );
        }
        Cmd::Sweep {
            target,
            component,
            rates,
            host_pods,
            duration,
            out_dir,
        } => {
            for rate in rates {
                let cfg = Config {
                    dapr_url: target.dapr_url.clone(),
                    app_id: target.app_id.clone(),
                    component: component.clone(),
                    path: "a".into(),
                    workload: target.workload.into(),
                    request_timeout_ms: target.request_timeout_ms,
                };
                let mode = Mode::Rate {
                    rate,
                    hosts: host_pods,
                    duration: Duration::from_secs(duration),
                };
                let out = out_dir.join(format!("sweep-rate{rate}.csv"));
                let rows = run(cfg, mode, &out).await?;
                let bad = rows.iter().filter(|r| r.status != 200).count();
                println!(
                    "rate {rate}: {} rows ({bad} non-200) -> {}",
                    rows.len(),
                    out.display()
                );
            }
        }
        Cmd::Report { input, out } => {
            let r = report::generate(&input, &out)?;
            for w in &r.warnings {
                eprintln!("{w}");
            }
            println!("{}", r.verdict);
            if !r.pass {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
