//! `earshot-worker` (T11.6, docs/sync.md section 10). Configured by environment variables
//! (`earshot_worker::config`); prints fixed words and problem words only, never a key, token,
//! pairing code or any meeting content.
//!
//! - `earshot-worker join` reads a pairing code from standard input (one line; never an argument,
//!   which `ps` and `docker inspect` would show), joins the vault of `EARSHOT_WORKER_RELAY` and
//!   stores the vault key and token in the vault files.
//! - `earshot-worker run` runs the worker until SIGTERM or Ctrl+C, then says gone.
//! - `earshot-worker health` is the container's health check (exit 0 when healthy).

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use earshot_runtime::sync::SyncTiming;
use earshot_worker::config::Config;
use earshot_worker::health;
use earshot_worker::worker::{Worker, WorkerError, join, open_services};

/// How long a stop waits for the gone presence to be sent.
const GONE_WAIT: Duration = Duration::from_secs(5);

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        Some("--version") => {
            println!("earshot-worker {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("health") => health_command(),
        Some("join") => report(join_command()),
        Some("run") => report(run_command()),
        _ => {
            eprintln!("usage: earshot-worker join | run | health | --version");
            ExitCode::from(64)
        }
    }
}

fn report(result: Result<(), WorkerError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("earshot-worker: {e}");
            ExitCode::FAILURE
        }
    }
}

fn health_command() -> ExitCode {
    let data = match Config::from_env() {
        Ok(c) => c.data,
        Err(e) => {
            eprintln!("unhealthy: {e}");
            return ExitCode::FAILURE;
        }
    };
    match health::check(&data, now_s()) {
        Ok(()) => {
            println!("healthy");
            ExitCode::SUCCESS
        }
        Err(u) => {
            println!("unhealthy: {}", u.word());
            ExitCode::FAILURE
        }
    }
}

fn join_command() -> Result<(), WorkerError> {
    let config = Config::from_env()?;
    config.relay()?;
    eprintln!("Type the pairing code shown on one of your devices, then press Enter:");
    let mut code = String::new();
    let read = std::io::stdin().read_line(&mut code);
    let services = open_services(&config.data)?;
    let joined = match read {
        Ok(_) => join(&services, &config, code.trim()),
        Err(_) => Err(WorkerError::Sync(
            earshot_runtime::sync::SyncProblem::BadCode,
        )),
    };
    earshot_net::wipe_string(&mut code);
    joined?;
    println!("joined");
    Ok(())
}

fn run_command() -> Result<(), WorkerError> {
    let config = Config::from_env()?;
    let data = config.data.clone();
    let services = open_services(&data)?;
    let worker = Worker::open(services, config)?;
    let thread = worker.start(SyncTiming::default())?;
    let heartbeat = health::heartbeat_path(&data);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| WorkerError::Spawn)?;
    println!("running");
    let stopped = runtime.block_on(async {
        let mut beat = tokio::time::interval(health::HEARTBEAT_EVERY);
        let mut terminate = terminate_signal()?;
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    if thread.ended() {
                        return Ok(false);
                    }
                    // A heartbeat that cannot be written makes the health check fail, which is
                    // what a full or read-only volume should do.
                    let _ = health::beat(&heartbeat, now_s());
                }
                _ = tokio::signal::ctrl_c() => return Ok(true),
                () = terminate.recv() => return Ok(true),
            }
        }
    });
    let _ = std::fs::remove_file(&heartbeat);
    worker.cancel();
    match stopped {
        Ok(true) => {
            if let Some(done) = thread.say_gone() {
                let _ = done.recv_timeout(GONE_WAIT);
            }
            thread.stop();
            println!("stopped");
            Ok(())
        }
        Ok(false) => {
            // The sync thread stopped on a problem the person must act on (removed, another
            // vault's key, an update needed) or on a panic: report its word and end.
            let status = earshot_runtime::sync::sync_status(&worker.services().store())
                .ok()
                .and_then(|s| s.problem)
                .unwrap_or(earshot_runtime::sync::SyncProblem::Internal);
            thread.stop();
            Err(WorkerError::Sync(status))
        }
        Err(e) => {
            thread.stop();
            Err(e)
        }
    }
}

/// SIGTERM (`docker stop`) on Unix; on other systems a future that never resolves (Ctrl+C
/// still stops it).
struct Terminate {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl Terminate {
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.signal.recv().await;
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    }
}

fn terminate_signal() -> Result<Terminate, WorkerError> {
    #[cfg(unix)]
    {
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| WorkerError::Spawn)?;
        Ok(Terminate { signal })
    }
    #[cfg(not(unix))]
    {
        Ok(Terminate {})
    }
}
