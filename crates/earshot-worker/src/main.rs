//! `earshot-worker` (T11.6, docs/sync.md section 10). Configured by environment variables
//! (`earshot_worker::config`); prints fixed words and problem words only, never a key, token,
//! pairing code or any meeting content.
//!
//! - `earshot-worker join` reads a pairing code from standard input (one line; never an argument,
//!   which `ps` and `docker inspect` would show), joins the vault of `EARSHOT_WORKER_RELAY` and
//!   stores the vault key and token in the vault files.
//! - `earshot-worker run` runs the worker until SIGTERM or Ctrl+C, then says gone. Not joined
//!   yet (neither vault file), it prints "waiting to join" once and looks for the files every
//!   [`earshot_worker::worker::JOIN_POLL`], so `docker exec -it earshot-worker earshot-worker
//!   join` in the running container joins it (T11.6d); a stop while waiting exits 0 and sends
//!   nothing.
//! - `earshot-worker health` is the container's health check (exit 0 when healthy).

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use earshot_runtime::sync::SyncTiming;
use earshot_worker::config::Config;
use earshot_worker::health;
use earshot_worker::worker::{Waited, Worker, WorkerError, join, open_services, wait_to_join};

/// How long a stop waits for the gone presence to be sent.
const GONE_WAIT: Duration = Duration::from_secs(5);

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn main() -> ExitCode {
    earshot_worker::crash::install_panic_hook();
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
    match health::check(&Config::data_from_env(), now_s()) {
        Ok(()) => {
            println!("healthy");
            ExitCode::SUCCESS
        }
        Err(u) => {
            println!("unhealthy: {}", u.describe());
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

/// [`run`], recording a run that cannot start or ends on a problem in the heartbeat file, so the
/// health check reports why while Docker's restart policy tries again.
fn run_command() -> Result<(), WorkerError> {
    let heartbeat = health::heartbeat_path(&Config::data_from_env());
    let ran = Config::from_env().map_err(WorkerError::from).and_then(run);
    if let Err(e) = &ran {
        let _ = health::record_stopped(&heartbeat, e.health_word());
    }
    ran
}

fn run(config: Config) -> Result<(), WorkerError> {
    let data = config.data.clone();
    let heartbeat = health::heartbeat_path(&data);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| WorkerError::Spawn)?;
    // One listener for the whole run, the wait and the run loop alike, so a stop that arrives
    // between two waits is not lost.
    let mut stop = {
        let _context = runtime.enter();
        StopSignals::listen()?
    };
    // Not joined yet: wait for `docker exec ... earshot-worker join` rather than exit and be
    // restarted. The store is not opened until the join is done.
    let _ = std::fs::create_dir_all(&data);
    let waited = wait_to_join(
        &config,
        &heartbeat,
        &now_s,
        &mut |notice| println!("{}", notice.line()),
        &mut |every| {
            runtime.block_on(async {
                tokio::select! {
                    () = tokio::time::sleep(every) => false,
                    () = stop.recv() => true,
                }
            })
        },
    )?;
    if waited == Waited::Stopped {
        // It never joined: no presence to take back, nothing to send.
        let _ = std::fs::remove_file(&heartbeat);
        println!("stopped");
        return Ok(());
    }
    let services = open_services(&data)?;
    let worker = Worker::open(services, config)?;
    let thread = worker.start(SyncTiming::default())?;
    println!("running");
    let stopped = runtime.block_on(async {
        let mut beat = tokio::time::interval(health::HEARTBEAT_EVERY);
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    if thread.ended() {
                        return false;
                    }
                    // A heartbeat that cannot be written makes the health check fail, which is
                    // what a full or read-only volume should do.
                    let _ = health::beat(&heartbeat, now_s());
                }
                () = stop.recv() => return true,
            }
        }
    });
    let _ = std::fs::remove_file(&heartbeat);
    worker.cancel();
    if stopped {
        if let Some(done) = thread.say_gone() {
            let _ = done.recv_timeout(GONE_WAIT);
        }
        thread.stop();
        println!("stopped");
        return Ok(());
    }
    // The sync thread stopped on a problem the person must act on (removed, another vault's
    // key, an update needed) or on a panic: report its word and end.
    let status = earshot_runtime::sync::sync_status(&worker.services().store())
        .ok()
        .and_then(|s| s.problem)
        .unwrap_or(earshot_runtime::sync::SyncProblem::Internal);
    thread.stop();
    Err(WorkerError::Sync(status))
}

/// The stop requests, registered once for the whole run: SIGTERM (`docker stop`) and SIGINT
/// (Ctrl+C) on Unix, Ctrl+C on Windows; elsewhere nothing. Signals that arrive while nothing
/// waits on them are kept until the next [`StopSignals::recv`].
struct StopSignals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl StopSignals {
    /// Registers the listeners (inside the runtime's context).
    fn listen() -> Result<Self, WorkerError> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                terminate: signal(SignalKind::terminate()).map_err(|_| WorkerError::Spawn)?,
                interrupt: signal(SignalKind::interrupt()).map_err(|_| WorkerError::Spawn)?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c().map_err(|_| WorkerError::Spawn)?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {})
        }
    }

    /// Resolves at the next stop request.
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.terminate.recv() => {}
                _ = self.interrupt.recv() => {}
            }
        }
        #[cfg(windows)]
        {
            self.ctrl_c.recv().await;
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::future::pending::<()>().await;
        }
    }
}
