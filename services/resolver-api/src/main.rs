use std::error::Error;

use qsy_resolver_api::{build_router, ApiConfig};
use resolver_core::DouyinResolver;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    init_tracing();
    let config = ApiConfig::from_env()?;
    let resolver = DouyinResolver::new()?;
    let app = build_router(&config, resolver).await?;
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(
        bind_addr = %config.bind_addr,
        body_limit_bytes = config.body_limit_bytes,
        max_in_flight = config.max_in_flight,
        download_dir = %config.download_dir.display(),
        "resolver API listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("qsy_resolver_api=info,tower_http=info"));
    let json =
        std::env::var("QSY_API_LOG_FORMAT").is_ok_and(|value| value.eq_ignore_ascii_case("json"));
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
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
