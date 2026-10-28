//! `dapr-sandbox-microvm`: the microVM sandbox driver, a Dapr pluggable
//! component serving `dapr.proto.components.v1.Sandbox` on a Unix socket.

use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use dapr_sandbox_microvm::backend::VmBackend;
use dapr_sandbox_microvm::backend::fake::FakeBackend;
use dapr_sandbox_microvm::service::{SandboxService, ServeError, serve};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

const DEFAULT_SOCKET: &str = "/tmp/dapr-components-sockets/microvm.sock";
const SOCKET_ENV: &str = "DAPR_COMPONENT_SOCKET";
/// How long in-flight calls may finish after a shutdown signal before the
/// pools are torn down regardless.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "\
usage: dapr-sandbox-microvm [--socket PATH] [--selftest IMAGE]

  --socket PATH      Unix socket to serve on (default: $DAPR_COMPONENT_SOCKET,
                     else /tmp/dapr-components-sockets/microvm.sock)
  --selftest IMAGE   boot one VM from IMAGE, run `echo ok`, print timings
  -h, --help         show this help

Logging: RUST_LOG (default: info).";

#[derive(Debug, Default)]
struct Args {
    socket: Option<PathBuf>,
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
            "--selftest" => args.selftest = Some(value(flag, inline)?),
            "-h" | "--help" => args.help = true,
            _ => return Err(format!("unknown argument {a:?}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
    if let Some(image) = args.selftest {
        // Task 11 implements it with the microsandbox backend.
        eprintln!(
            "error: --selftest {image}: not available in this build: it needs the microsandbox \
             backend, which is not implemented yet"
        );
        return ExitCode::from(2);
    }

    let socket = args
        .socket
        .or_else(|| std::env::var_os(SOCKET_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));

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
    match runtime.block_on(run(&socket)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn backend() -> Arc<dyn VmBackend> {
    if cfg!(feature = "microsandbox") {
        warn!(
            "feature `microsandbox` is enabled but the real backend is not yet available; \
             serving the in-memory FakeBackend (no VMs, no isolation)"
        );
    } else {
        warn!("serving the in-memory FakeBackend (no VMs, no isolation)");
    }
    Arc::new(FakeBackend::new())
}

async fn run(socket: &Path) -> Result<(), ServeError> {
    // Handle signals from the start, so one during startup still cleans up.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let service = SandboxService::new(backend());
    // Listen at once: daprd discovers pluggable components when it starts.
    // Pools fill in the background after Init.
    let listener = bind(socket)?;
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
    if let Err(e) = std::fs::remove_file(socket)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(socket = %socket.display(), "removing socket: {e}");
    }
    info!("stopped");
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args() {
        let a = parse(&[]).unwrap();
        assert!(a.socket.is_none() && a.selftest.is_none() && !a.help);
        let a = parse(&["--socket", "/x.sock"]).unwrap();
        assert_eq!(a.socket, Some(PathBuf::from("/x.sock")));
        let a = parse(&["--socket=/y.sock", "--selftest=img:1"]).unwrap();
        assert_eq!(a.socket, Some(PathBuf::from("/y.sock")));
        assert_eq!(a.selftest.as_deref(), Some("img:1"));
        let a = parse(&["--selftest", "python:3.12-slim"]).unwrap();
        assert_eq!(a.selftest.as_deref(), Some("python:3.12-slim"));
        assert!(parse(&["-h"]).unwrap().help);
        assert!(parse(&["--socket"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--bogus=1"]).is_err());
        assert!(parse(&["positional"]).is_err());
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
