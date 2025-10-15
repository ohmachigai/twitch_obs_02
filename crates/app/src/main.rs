mod router;
mod tap;
mod telemetry;

use std::net::SocketAddr;

use tracing::info;
use twi_overlay_util::{load_env_file, AppConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_env_file();
    let config = AppConfig::from_env()?;

    telemetry::init_tracing(&config)?;
    let metrics = telemetry::init_metrics()?;

    let tap_hub = tap::TapHub::new();
    if config.environment.is_development() {
        tap_hub.spawn_mock_publisher();
    }

    let state = router::AppState::new(metrics, tap_hub.clone());

    let addr: SocketAddr = config.bind_addr;
    info!(stage = "app", %addr, env = %config.environment.as_str(), "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router::app_router(state))
        .await
        .map_err(|err| err.into())
}
