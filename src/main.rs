use clap::Parser;

mod config;
mod error;
mod grep;
mod http;
mod limits;
mod logfile;
mod pathguard;
mod rotate;
mod watcher;

use config::Config;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();
    if let Err(e) = config.validate() {
        eprintln!("error: {e}");
        std::process::exit(2);
    }

    let root = match std::fs::canonicalize(&config.root) {
        Ok(r) if r.is_dir() => r,
        Ok(_) => {
            eprintln!(
                "error: --root is not a directory: {}",
                config.root.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: cannot access --root {}: {e}", config.root.display());
            std::process::exit(1);
        }
    };

    let addr = match config.socket_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let state = http::AppState::from_config(&config, root);
    let app = http::build_router(state);

    // Two worker threads + a right-sized blocking pool: the sidecar must not
    // hog the host; grep/read work runs on the blocking pool, sized so every
    // permitted search plus the tail/list blocking calls always have a thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(config.max_searches + 4)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("error: cannot bind {addr}: {e}");
                std::process::exit(1);
            });
        tracing::info!("log-sidecar listening on {addr}");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("server error");
        tracing::info!("shutdown complete");
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl_c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
