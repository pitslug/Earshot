//! `earshot-sync-server`: the sync relay (docs/sync-spec.md section 4).
//!
//! Configuration by environment only: `EARSHOT_SYNC_SETUP_SECRET` (required, at least 24
//! characters), `EARSHOT_SYNC_DATA` (default `/data`), `EARSHOT_SYNC_LISTEN` (default
//! `0.0.0.0:8787`). TLS is the reverse proxy's.

use std::net::SocketAddr;
use std::process::ExitCode;

use earshot_sync_server::{Config, Relay};

#[tokio::main]
async fn main() -> ExitCode {
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
