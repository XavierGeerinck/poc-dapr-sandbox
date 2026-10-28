//! `dapr-sandbox-microvm`: the microVM sandbox driver, a Dapr pluggable
//! component serving `dapr.proto.components.v1.Sandbox` on a Unix socket.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use dapr_sandbox_microvm::backend::VmBackend;
use dapr_sandbox_microvm::backend::fake::FakeBackend;
use dapr_sandbox_microvm::pool::PoolOptions;
use dapr_sandbox_microvm::service::{SandboxService, ServeError, serve};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

const DEFAULT_SOCKET: &str = "/tmp/dapr-components-sockets/microvm.sock";
const SOCKET_ENV: &str = "DAPR_COMPONENT_SOCKET";
const MSB_HOME_ENV: &str = "DAPR_SANDBOX_MSB_HOME";
const STATE_DIR_ENV: &str = "DAPR_SANDBOX_STATE_DIR";
/// The driver's directory under the XDG state root.
const APP_DIR: &str = "dapr-sandbox-microvm";
/// A shorter one, for when `<state root>/APP_DIR/msb` is too long a
/// microsandbox home (see [`resolve_paths`]).
const SHORT_APP_DIR: &str = "dsbm";
/// How long in-flight calls may finish after a shutdown signal before the
/// pools are torn down regardless.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Logging without `RUST_LOG`: the driver at info, its dependencies (whose
/// info level is chatty) at warn.
const DEFAULT_LOG: &str = "warn,dapr_sandbox_microvm=info";

const USAGE: &str = "\
usage: dapr-sandbox-microvm [--socket PATH] [--state-dir DIR] [--msb-home DIR]
                            [--backend microsandbox|fake] [--selftest IMAGE]

  --socket PATH      Unix socket to serve on (default: $DAPR_COMPONENT_SOCKET,
                     else /tmp/dapr-components-sockets/microvm.sock)
  --state-dir DIR    where each component's Init properties are kept, to
                     restore its pool after a restart (default:
                     $DAPR_SANDBOX_STATE_DIR, else <base>/components)
  --msb-home DIR     microsandbox home: VMs, images, snapshots, sockets; at most
                     51 bytes on macOS, 55 on Linux (default:
                     $DAPR_SANDBOX_MSB_HOME, else <base>/msb)
  --backend KIND     microsandbox (default when built with it) or fake (in-memory,
                     no VMs, no isolation; for tests and smoke runs)
  --selftest IMAGE   boot one VM from IMAGE, run `echo ok`, print timings, exit
  -h, --help         show this help

<base> is $XDG_STATE_HOME/dapr-sandbox-microvm, else
~/.local/state/dapr-sandbox-microvm; if <base>/msb is too long a microsandbox
home, <base> is <state root>/dsbm instead. The ambient MSB_HOME is ignored: the
driver sets it to its microsandbox home.

Logging: RUST_LOG (default: warn,dapr_sandbox_microvm=info; microsandbox logs
every exec connection at info).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Microsandbox,
    Fake,
}

impl BackendKind {
    const DEFAULT: Self = if cfg!(feature = "microsandbox") {
        Self::Microsandbox
    } else {
        Self::Fake
    };
}

#[derive(Debug, Default)]
struct Args {
    socket: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    msb_home: Option<PathBuf>,
    backend: Option<BackendKind>,
    selftest: Option<String>,
    help: bool,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    while let Some(a) = it.next() {
        let mut value = |flag: &str, inline: Option<&str>| -> Result<String, String> {
            match inline {
                Some(v) => Ok(v.to_string()),
                None => it.next().ok_or_else(|| format!("{flag} needs a value")),
            }
        };
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        match flag {
            "--socket" => args.socket = Some(value(flag, inline)?.into()),
            "--state-dir" => args.state_dir = Some(value(flag, inline)?.into()),
            "--msb-home" => args.msb_home = Some(value(flag, inline)?.into()),
            "--backend" => {
                args.backend = Some(match value(flag, inline)?.as_str() {
                    "microsandbox" => BackendKind::Microsandbox,
                    "fake" => BackendKind::Fake,
                    other => return Err(format!("--backend: unknown backend {other:?}")),
                })
            }
            "--selftest" => args.selftest = Some(value(flag, inline)?),
            "-h" | "--help" => args.help = true,
            _ => return Err(format!("unknown argument {a:?}")),
        }
    }
    Ok(args)
}

/// Where the driver keeps its state.
#[derive(Debug, PartialEq, Eq)]
struct Paths {
    state_dir: PathBuf,
    /// `None` when no microsandbox home is needed (fake backend).
    msb_home: Option<PathBuf>,
}

/// Resolves the state directory and, if `msb_max_len` is set (the
/// microsandbox backend), the microsandbox home: a flag, else its
/// environment variable, else `<base>/components` and `<base>/msb`.
///
/// `<base>` is `<state root>/dapr-sandbox-microvm`, where the state root is
/// `$XDG_STATE_HOME` (if absolute) or `$HOME/.local/state`. microsandbox
/// derives Unix socket paths from its home, so a default home longer than
/// `msb_max_len` bytes switches `<base>` to `<state root>/dsbm`, keeping
/// both under one base; if even that is too long, an explicit home is
/// required. Relative paths are made absolute.
fn resolve_paths(
    args: &Args,
    env: impl Fn(&str) -> Option<OsString>,
    msb_max_len: Option<usize>,
) -> Result<Paths, String> {
    let nonempty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let explicit_state = args.state_dir.clone().or_else(|| nonempty(STATE_DIR_ENV));
    let explicit_msb = args.msb_home.clone().or_else(|| nonempty(MSB_HOME_ENV));
    let shorten = msb_max_len.filter(|_| explicit_msb.is_none());
    let base = || -> Result<PathBuf, String> {
        let root = nonempty("XDG_STATE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| nonempty("HOME").map(|h| h.join(".local").join("state")))
            .ok_or_else(|| {
                format!(
                    "no state directory: set $HOME or $XDG_STATE_HOME, or --state-dir \
                     ({STATE_DIR_ENV}) and --msb-home ({MSB_HOME_ENV})"
                )
            })?;
        let long = root.join(APP_DIR);
        let Some(max) = shorten else {
            return Ok(long);
        };
        let short = root.join(SHORT_APP_DIR);
        [long, short]
            .into_iter()
            .find(|b| b.join("msb").as_os_str().len() <= max)
            .ok_or_else(|| {
                format!(
                    "the default microsandbox home {} is too long: microsandbox puts Unix \
                     sockets under it, so it must be at most {max} bytes; set --msb-home \
                     ({MSB_HOME_ENV}) to a shorter directory",
                    root.join(APP_DIR).join("msb").display()
                )
            })
    };
    let absolute =
        |p: PathBuf| std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()));
    let state_dir = match explicit_state {
        Some(dir) => dir,
        None => base()?.join("components"),
    };
    let msb_home = match (msb_max_len, explicit_msb) {
        (None, _) => None,
        (Some(_), Some(home)) => Some(home),
        (Some(_), None) => Some(base()?.join("msb")),
    };
    Ok(Paths {
        state_dir: absolute(state_dir)?,
        msb_home: msb_home.map(absolute).transpose()?,
    })
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG)),
        )
        .with_writer(io::stderr)
        .init();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let kind = args.backend.unwrap_or(BackendKind::DEFAULT);
    if kind == BackendKind::Microsandbox && !cfg!(feature = "microsandbox") {
        eprintln!("error: --backend microsandbox: this build lacks feature `microsandbox`");
        return ExitCode::from(2);
    }
    if args.selftest.is_some() && kind != BackendKind::Microsandbox {
        eprintln!(
            "error: --selftest: not available with the fake backend{}",
            if cfg!(feature = "microsandbox") {
                ""
            } else {
                " (this build lacks feature `microsandbox`)"
            }
        );
        return ExitCode::from(2);
    }

    let msb_max_len = match kind {
        BackendKind::Fake => None,
        BackendKind::Microsandbox => msb_max_len(),
    };
    let paths = match resolve_paths(&args, |k| std::env::var_os(k), msb_max_len) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(home) = &paths.msb_home {
        // microsandbox reads MSB_HOME for its ambient defaults, and passes
        // the environment on to the `msb` processes it starts: make both
        // agree with the home the backend uses. Ignore any inherited value.
        // SAFETY: no other thread exists yet (the runtime starts below).
        unsafe { std::env::set_var("MSB_HOME", home) };
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(image) = args.selftest {
        let home = paths.msb_home.expect("the microsandbox backend has a home");
        return runtime.block_on(selftest::run(&image, home));
    }

    let socket = args
        .socket
        .or_else(|| std::env::var_os(SOCKET_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    match runtime.block_on(run(&socket, kind, paths)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "microsandbox")]
fn msb_max_len() -> Option<usize> {
    Some(dapr_sandbox_microvm::microsandbox_backend::MAX_MSB_HOME_LEN)
}

#[cfg(not(feature = "microsandbox"))]
fn msb_max_len() -> Option<usize> {
    None
}

async fn backend(kind: BackendKind, paths: &Paths) -> Result<Arc<dyn VmBackend>, ServeError> {
    match (kind, &paths.msb_home) {
        #[cfg(feature = "microsandbox")]
        (BackendKind::Microsandbox, Some(home)) => {
            let b =
                dapr_sandbox_microvm::microsandbox_backend::MicrosandboxBackend::new(home.clone())
                    .await?;
            Ok(Arc::new(b))
        }
        (BackendKind::Microsandbox, _) => Err("the microsandbox backend is not available".into()),
        (BackendKind::Fake, _) => {
            warn!("serving the in-memory FakeBackend (no VMs, no isolation)");
            Ok(Arc::new(FakeBackend::new()))
        }
    }
}

async fn run(socket: &Path, kind: BackendKind, paths: Paths) -> Result<(), ServeError> {
    // Handle signals from the start, so one during startup still cleans up.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Listen at once: daprd discovers pluggable components when it starts.
    // Connections wait in the backlog until the backend is up; pools fill
    // in the background after Init.
    let listener = bind(socket)?;
    info!(socket = %socket.display(), state_dir = %paths.state_dir.display(),
        msb_home = ?paths.msb_home, "starting");
    let backend = tokio::select! {
        b = backend(kind, &paths) => match b {
            Ok(b) => b,
            Err(e) => {
                remove_socket(socket)?;
                return Err(e);
            }
        },
        _ = sigterm.recv() => { info!("SIGTERM during startup"); return remove_socket(socket); }
        _ = sigint.recv() => { info!("SIGINT during startup"); return remove_socket(socket); }
    };
    let service = SandboxService::with_state_dir(backend, PoolOptions::default(), &paths.state_dir);
    // Before serving, so that no Init races it.
    let restored = service.restore_persisted();
    if restored > 0 {
        info!(count = restored, "restored persisted components");
    }
    info!(socket = %socket.display(), "serving dapr.proto.components.v1.Sandbox");

    let (stop, stopped) = oneshot::channel::<()>();
    let mut server = tokio::spawn(serve(listener, service.clone(), async {
        let _ = stopped.await;
    }));

    let served_early = tokio::select! {
        _ = sigterm.recv() => { info!("SIGTERM: shutting down"); None }
        _ = sigint.recv() => { info!("SIGINT: shutting down"); None }
        res = &mut server => Some(res),
    };

    let result = match served_early {
        Some(res) => res.map_err(ServeError::from).and_then(|r| r),
        None => {
            // Stop accepting calls, let in-flight ones finish (bounded).
            let _ = stop.send(());
            match tokio::time::timeout(DRAIN_TIMEOUT, &mut server).await {
                Ok(res) => res.map_err(ServeError::from).and_then(|r| r),
                Err(_) => {
                    warn!(timeout = ?DRAIN_TIMEOUT, "in-flight calls did not finish; aborting them");
                    server.abort();
                    Ok(())
                }
            }
        }
    };

    // Destroy every VM before exiting; a second signal exits at once.
    tokio::select! {
        _ = service.shutdown() => {}
        _ = sigterm.recv() => warn!("SIGTERM again: exiting without waiting for VM teardown"),
        _ = sigint.recv() => warn!("SIGINT again: exiting without waiting for VM teardown"),
    }
    remove_socket(socket)?;
    info!("stopped");
    result
}

fn remove_socket(socket: &Path) -> Result<(), ServeError> {
    if let Err(e) = std::fs::remove_file(socket)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(socket = %socket.display(), "removing socket: {e}");
    }
    Ok(())
}

/// Binds the socket, creating its directory and replacing a stale socket
/// file. Refuses to replace a live socket or a file that is not a socket.
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "{}: another process is serving on this socket",
                        path.display()
                    ),
                ));
            }
            info!(socket = %path.display(), "removing stale socket");
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    UnixListener::bind(path)
}

#[cfg(feature = "microsandbox")]
mod selftest {
    //! `--selftest IMAGE`: one template, one restored VM, `echo ok`, timings.

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use dapr_sandbox_microvm::backend::{ExecSpec, VmBackend};
    use dapr_sandbox_microvm::config::Config;
    use dapr_sandbox_microvm::microsandbox_backend::MicrosandboxBackend;
    use dapr_sandbox_microvm::pool::{template_name, vm_name, vm_prefix};

    /// The component name the selftest's VMs and template derive from.
    const COMPONENT: &str = "dapr-sandbox-microvm-selftest";
    const RUNS: usize = 20;

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    fn percentile(samples: &[Duration], p: f64) -> Duration {
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

    pub async fn run(image: &str, msb_home: PathBuf) -> ExitCode {
        println!(
            "selftest: image {image}, microsandbox home {}",
            msb_home.display()
        );
        let start = Instant::now();
        let b = match MicrosandboxBackend::new(msb_home).await {
            Ok(b) => b,
            Err(e) => {
                println!("selftest: FAILED: {e}");
                return ExitCode::FAILURE;
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
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("selftest: FAILED: {e}");
                ExitCode::FAILURE
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
}

#[cfg(not(feature = "microsandbox"))]
mod selftest {
    use std::path::PathBuf;
    use std::process::ExitCode;

    pub async fn run(_image: &str, _msb_home: PathBuf) -> ExitCode {
        unreachable!("main rejects --selftest without the microsandbox backend")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args() {
        let a = parse(&[]).unwrap();
        assert!(a.socket.is_none() && a.selftest.is_none() && !a.help);
        assert!(a.state_dir.is_none() && a.msb_home.is_none() && a.backend.is_none());
        let a = parse(&["--socket", "/x.sock"]).unwrap();
        assert_eq!(a.socket, Some(PathBuf::from("/x.sock")));
        let a = parse(&["--socket=/y.sock", "--selftest=img:1"]).unwrap();
        assert_eq!(a.socket, Some(PathBuf::from("/y.sock")));
        assert_eq!(a.selftest.as_deref(), Some("img:1"));
        let a = parse(&["--selftest", "python:3.12-slim"]).unwrap();
        assert_eq!(a.selftest.as_deref(), Some("python:3.12-slim"));
        let a = parse(&["--state-dir", "/s", "--msb-home=/m", "--backend", "fake"]).unwrap();
        assert_eq!(a.state_dir, Some(PathBuf::from("/s")));
        assert_eq!(a.msb_home, Some(PathBuf::from("/m")));
        assert_eq!(a.backend, Some(BackendKind::Fake));
        let a = parse(&["--backend=microsandbox"]).unwrap();
        assert_eq!(a.backend, Some(BackendKind::Microsandbox));
        assert!(parse(&["--backend", "qemu"]).is_err());
        assert!(parse(&["-h"]).unwrap().help);
        assert!(parse(&["--socket"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--bogus=1"]).is_err());
        assert!(parse(&["positional"]).is_err());
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| vars.get(k).cloned()
    }

    fn paths(state: &str, msb: Option<&str>) -> Paths {
        Paths {
            state_dir: PathBuf::from(state),
            msb_home: msb.map(PathBuf::from),
        }
    }

    #[test]
    fn default_paths() {
        let none = Args::default();
        let home = env(&[("HOME", "/home/u")]);
        // The fake backend needs no microsandbox home.
        assert_eq!(
            resolve_paths(&none, &home, None).unwrap(),
            paths("/home/u/.local/state/dapr-sandbox-microvm/components", None)
        );
        assert_eq!(
            resolve_paths(&none, &home, Some(51)).unwrap(),
            paths(
                "/home/u/.local/state/dapr-sandbox-microvm/components",
                Some("/home/u/.local/state/dapr-sandbox-microvm/msb")
            )
        );
        // XDG_STATE_HOME wins if absolute.
        let xdg = env(&[("HOME", "/home/u"), ("XDG_STATE_HOME", "/st")]);
        assert_eq!(
            resolve_paths(&none, &xdg, Some(51)).unwrap(),
            paths(
                "/st/dapr-sandbox-microvm/components",
                Some("/st/dapr-sandbox-microvm/msb")
            )
        );
        let rel = env(&[("HOME", "/home/u"), ("XDG_STATE_HOME", "st")]);
        assert_eq!(
            resolve_paths(&none, &rel, None).unwrap().state_dir,
            PathBuf::from("/home/u/.local/state/dapr-sandbox-microvm/components")
        );
        // No HOME and no XDG_STATE_HOME: needs explicit paths.
        assert!(resolve_paths(&none, env(&[]), None).is_err());
    }

    #[test]
    fn long_default_msb_home_falls_back_to_a_shorter_base() {
        let none = Args::default();
        // As on the development Mac: <base>/msb is 59 bytes.
        let home = env(&[("HOME", "/Users/xaviergeerinck")]);
        assert_eq!(
            resolve_paths(&none, &home, Some(51)).unwrap(),
            paths(
                "/Users/xaviergeerinck/.local/state/dsbm/components",
                Some("/Users/xaviergeerinck/.local/state/dsbm/msb")
            )
        );
        // Under a limit it fits, the long base stays.
        let p = resolve_paths(&none, &home, Some(59)).unwrap();
        assert_eq!(
            p.msb_home.unwrap(),
            PathBuf::from("/Users/xaviergeerinck/.local/state/dapr-sandbox-microvm/msb")
        );
        // Too long for either base.
        let long = env(&[("HOME", "/home/a-very-long-user-name-for-the-test")]);
        let e = resolve_paths(&none, &long, Some(51)).unwrap_err();
        assert!(e.contains("--msb-home"), "{e}");
        // An explicit home is not shortened; the state dir keeps the long base.
        let a = Args {
            msb_home: Some("/m".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_paths(&a, &long, Some(51)).unwrap(),
            paths(
                "/home/a-very-long-user-name-for-the-test/.local/state/dapr-sandbox-microvm/components",
                Some("/m")
            )
        );
    }

    #[test]
    fn explicit_paths() {
        let vars = env(&[
            ("HOME", "/home/u"),
            (STATE_DIR_ENV, "/env-state"),
            (MSB_HOME_ENV, "/env-msb"),
        ]);
        assert_eq!(
            resolve_paths(&Args::default(), &vars, Some(51)).unwrap(),
            paths("/env-state", Some("/env-msb"))
        );
        // Flags win over the environment; empty variables are unset.
        let a = Args {
            state_dir: Some("/flag-state".into()),
            msb_home: Some("/flag-msb".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_paths(&a, &vars, Some(51)).unwrap(),
            paths("/flag-state", Some("/flag-msb"))
        );
        let empty = env(&[("HOME", "/home/u"), (STATE_DIR_ENV, ""), (MSB_HOME_ENV, "")]);
        assert_eq!(
            resolve_paths(&Args::default(), &empty, Some(51)).unwrap(),
            paths(
                "/home/u/.local/state/dapr-sandbox-microvm/components",
                Some("/home/u/.local/state/dapr-sandbox-microvm/msb")
            )
        );
        // Explicit paths need no HOME; relative ones become absolute.
        let a = Args {
            state_dir: Some("rel".into()),
            msb_home: Some("/m".into()),
            ..Default::default()
        };
        let p = resolve_paths(&a, env(&[]), Some(51)).unwrap();
        assert_eq!(p.state_dir, std::env::current_dir().unwrap().join("rel"));
    }

    #[test]
    fn bind_refuses_non_sockets_and_live_sockets() {
        let dir = tempfile::Builder::new().prefix("dsb").tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        assert_eq!(
            bind(&file).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        let live = dir.path().join("live.sock");
        let _l = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert_eq!(bind(&live).unwrap_err().kind(), io::ErrorKind::AddrInUse);

        // Creates missing directories.
        let nested = dir.path().join("a/b/s.sock");
        drop(bind(&nested).unwrap());
        assert!(nested.exists());
    }
}
