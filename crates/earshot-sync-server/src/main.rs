//! `earshot-sync-server`: the sync relay (docs/sync-spec.md section 4).
//!
//! Configuration by environment only: `EARSHOT_SYNC_SETUP_SECRET` or
//! `EARSHOT_SYNC_SETUP_SECRET_FILE` (required, at least 24 characters), `EARSHOT_SYNC_DATA`
//! (default `/data`), `EARSHOT_SYNC_LISTEN` (default `0.0.0.0:8787`). TLS is the reverse
//! proxy's.
//!
//! `earshot-sync-server health` is the container's health check: it connects to the relay's
//! port and exits 0 when something accepts, 1 otherwise. It needs no secret.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use earshot_sync_server::{Config, Relay, health_target, listen_of};

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("health") {
        let var = |k: &str| std::env::var(k).ok();
        let Ok(listen) = listen_of(&var) else {
            return ExitCode::from(1);
        };
        return match std::net::TcpStream::connect_timeout(
            &health_target(listen),
            Duration::from_secs(3),
        ) {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    let config = match Config::from_env(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("earshot-sync-server: {e}");
            return ExitCode::from(2);
        }
    };
    let relay = match Relay::open(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("earshot-sync-server: cannot open the data folder: {e}");
            return ExitCode::from(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "earshot-sync-server: cannot listen on {}: {e}",
                config.listen
            );
            return ExitCode::from(1);
        }
    };
    println!(
        "earshot-sync-server {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        config.listen
    );
    let app =
        earshot_sync_server::router(relay).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("earshot-sync-server: {e}");
        return ExitCode::from(1);
    }
    println!("earshot-sync-server stopped");
    ExitCode::SUCCESS
}

/// Ctrl+C, or SIGTERM where there is one (`docker stop`).
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
